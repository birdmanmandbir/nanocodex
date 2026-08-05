//! Small audited bridge for APIs whose C value wrappers are missing from the
//! safe `accessibility` crate.

#![cfg(target_os = "macos")]

use std::{
    cell::Cell,
    collections::VecDeque,
    ffi::c_void,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use accessibility::{AXAttribute, AXUIElement};
use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXObserverAddNotification, AXObserverCreate,
    AXObserverGetRunLoopSource, AXObserverGetTypeID, AXObserverRef, AXUIElementRef, AXValueGetType,
    AXValueGetTypeID, AXValueGetValue, AXValueRef, kAXCreatedNotification,
    kAXElementBusyChangedNotification, kAXFocusedUIElementChangedNotification,
    kAXFocusedWindowChangedNotification, kAXLayoutChangedNotification,
    kAXMainWindowChangedNotification, kAXMovedNotification, kAXPositionAttribute,
    kAXResizedNotification, kAXRowCountChangedNotification, kAXSelectedChildrenChangedNotification,
    kAXSelectedTextChangedNotification, kAXSizeAttribute, kAXTitleChangedNotification,
    kAXTrustedCheckOptionPrompt, kAXUIElementDestroyedNotification, kAXValueChangedNotification,
    kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowCreatedNotification,
};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    boolean::CFBoolean,
    declare_TCFType,
    dictionary::CFDictionary,
    impl_CFTypeDescription, impl_TCFType,
    number::CFNumber,
    runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopDefaultMode},
    string::{CFString, CFStringRef},
};
use core_graphics::{
    access::ScreenCaptureAccess,
    base::{kCGBitmapByteOrder32Big, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::CGContext,
    event::{
        CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult, EventField,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::{CGPoint, CGRect, CGSize},
    window::{
        copy_window_info, create_image, kCGNullWindowID, kCGWindowBounds,
        kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming, kCGWindowIsOnscreen,
        kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
        kCGWindowListOptionIncludingWindow, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
        kCGWindowOwnerPID,
    },
};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBitmapImageRep, NSColor, NSCursor,
    NSDeviceRGBColorSpace, NSEventMask, NSFloatingWindowLevel, NSImage, NSImageScaling,
    NSImageView, NSPanel, NSRunningApplication, NSScreen, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSData, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};

declare_TCFType!(AccessibilityObserver, AXObserverRef);
impl_TCFType!(AccessibilityObserver, AXObserverRef, AXObserverGetTypeID);
impl_CFTypeDescription!(AccessibilityObserver);

/// Result of requesting richer renderer-backed accessibility trees.
pub struct AccessibilityEnablement {
    pub manual: bool,
    pub enhanced: bool,
}

/// Enables public manual/enhanced Accessibility attributes when an app
/// supports them. Unsupported native apps simply report `false`.
pub fn enable_application_accessibility(application: &AXUIElement) -> AccessibilityEnablement {
    let manual = AXAttribute::<CFType>::new(&CFString::new("AXManualAccessibility"));
    let enhanced = AXAttribute::<CFType>::new(&CFString::new("AXEnhancedUserInterface"));
    let true_value = CFBoolean::true_value();
    // SAFETY: `CFBoolean::true_value()` is a valid immortal Core Foundation
    // object. Wrapping under the get rule retains it as a type-erased value.
    let true_value = unsafe { CFType::wrap_under_get_rule(true_value.as_CFTypeRef()) };
    AccessibilityEnablement {
        manual: application
            .set_attribute(&manual, true_value.clone())
            .is_ok(),
        enhanced: application.set_attribute(&enhanced, true_value).is_ok(),
    }
}

const SYNTHETIC_MARKER: i64 = 0x004e_414e_4f43_4458;
const MAX_CAPTURE_PIXELS: usize = 25_000_000;
static ACCESSIBILITY_REQUESTED: AtomicBool = AtomicBool::new(false);
static SCREEN_CAPTURE_REQUESTED: AtomicBool = AtomicBool::new(false);
static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Result of checking a TCC permission and, when needed, requesting it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionRequest {
    /// The process can use the protected API now.
    Granted,
    /// macOS was asked to present its permission UI. The caller may need to
    /// relaunch after the user grants access.
    Prompted,
}

/// Returns whether the current process may inspect and operate accessibility elements.
#[must_use]
pub fn accessibility_trusted() -> bool {
    // SAFETY: AXIsProcessTrusted accepts no pointers and has no ownership effect.
    unsafe { AXIsProcessTrusted() }
}

/// Checks Accessibility trust and asks macOS to present its standard prompt
/// when access has not been granted yet.
#[must_use]
pub fn request_accessibility() -> PermissionRequest {
    if accessibility_trusted() {
        return PermissionRequest::Granted;
    }
    if ACCESSIBILITY_REQUESTED.swap(true, Ordering::AcqRel) {
        return PermissionRequest::Prompted;
    }
    // SAFETY: The dictionary owns both retained CoreFoundation values for the
    // duration of the call. The option is the documented public prompt key.
    let prompt_key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let options = CFDictionary::from_CFType_pairs(&[(prompt_key, CFBoolean::true_value())]);
    let granted = unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
    if granted {
        PermissionRequest::Granted
    } else {
        PermissionRequest::Prompted
    }
}

/// Checks Screen Recording access and asks macOS to present its standard
/// request UI when the current process is not registered yet.
#[must_use]
pub fn request_screen_capture() -> PermissionRequest {
    let access = ScreenCaptureAccess;
    if access.preflight() {
        PermissionRequest::Granted
    } else if SCREEN_CAPTURE_REQUESTED.swap(true, Ordering::AcqRel) {
        PermissionRequest::Prompted
    } else if access.request() {
        PermissionRequest::Granted
    } else {
        PermissionRequest::Prompted
    }
}

/// Reads an element's global position and size through retained CoreFoundation values.
#[must_use]
pub fn element_rect(element: &AXUIElement) -> Option<(f64, f64, f64, f64)> {
    let position = value_attribute::<CGPoint>(
        element,
        kAXPositionAttribute,
        kAXValueTypeCGPoint,
        CGPoint::new(0.0, 0.0),
    )?;
    let size = value_attribute::<CGSize>(
        element,
        kAXSizeAttribute,
        kAXValueTypeCGSize,
        CGSize::new(0.0, 0.0),
    )?;
    Some((position.x, position.y, size.width, size.height))
}

/// Marks a generated event so the intervention observer does not mistake it
/// for human input.
pub fn mark_synthetic(event: &CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, SYNTHETIC_MARKER);
}

/// Public data for one layer-zero CoreGraphics window.
#[derive(Clone, Debug)]
pub struct NativeWindow {
    pub id: u32,
    pub pid: i32,
    pub owner_name: String,
    pub bundle_id: Option<String>,
    pub title: Option<String>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub on_screen: bool,
}

/// A captured window rendered into tightly packed RGBA8 pixels.
pub struct NativeImage {
    pub data: NativeImageData,
    pub width: u32,
    pub height: u32,
}

/// Pixel storage returned by a native window capture.
pub enum NativeImageData {
    /// Tightly packed RGBA8 pixels from the in-process Quartz fallback.
    Rgba(Vec<u8>),
    /// A complete PNG emitted by the macOS screenshot service.
    Png(Vec<u8>),
}

/// A bounded native window-capture failure.
#[derive(Debug)]
pub struct CaptureWindowError {
    message: String,
}

impl std::fmt::Display for CaptureWindowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CaptureWindowError {}

/// Enumerates all non-desktop layer-zero windows with their owning application.
#[must_use]
pub fn windows() -> Vec<NativeWindow> {
    native_windows(
        kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )
}

/// Looks up one layer-zero CoreGraphics window without enumerating every application.
#[must_use]
pub fn window(window_id: u32) -> Option<NativeWindow> {
    native_windows(kCGWindowListOptionIncludingWindow, window_id)
        .into_iter()
        .find(|window| window.id == window_id)
}

fn native_windows(options: u32, relative_to: u32) -> Vec<NativeWindow> {
    let Some(raw) = copy_window_info(options, relative_to) else {
        return Vec::new();
    };
    let reference = raw.as_concrete_TypeRef();
    std::mem::forget(raw);
    // SAFETY: Quartz documents every element returned by
    // CGWindowListCopyWindowInfo as CFDictionary<CFString, CFType>.
    let dictionaries =
        unsafe { CFArray::<CFDictionary<CFString, CFType>>::wrap_under_create_rule(reference) };
    dictionaries
        .iter()
        .filter_map(|dictionary| native_window(&dictionary))
        .collect()
}

fn native_window(dictionary: &CFDictionary<CFString, CFType>) -> Option<NativeWindow> {
    let layer = number(dictionary, unsafe { kCGWindowLayer })?.to_i32()?;
    if layer != 0 {
        return None;
    }
    let pid = number(dictionary, unsafe { kCGWindowOwnerPID })?.to_i32()?;
    let id = u32::try_from(number(dictionary, unsafe { kCGWindowNumber })?.to_i64()?).ok()?;
    let bounds = dictionary_value(dictionary, unsafe { kCGWindowBounds })?;
    let bounds = bounds.downcast::<CFDictionary>()?;
    let frame = CGRect::from_dict_representation(&bounds)?;
    let owner_name = string(dictionary, unsafe { kCGWindowOwnerName })?;
    let title = string(dictionary, unsafe { kCGWindowName }).filter(|value| !value.is_empty());
    let on_screen = boolean(dictionary, unsafe { kCGWindowIsOnscreen }).unwrap_or(false);
    let (localized_name, bundle_id) = running_application(pid);
    Some(NativeWindow {
        id,
        pid,
        owner_name: localized_name.unwrap_or(owner_name),
        bundle_id,
        title,
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
        on_screen,
    })
}

/// Captures one window by ID without compositing other windows above it.
pub fn capture_window(window_id: u32) -> Result<NativeImage, CaptureWindowError> {
    capture_window_service(window_id).or_else(|service_error| {
        capture_window_legacy(window_id).ok_or_else(|| CaptureWindowError {
            message: format!(
                "macOS screenshot service failed ({service_error}); the in-process Quartz fallback also returned no image"
            ),
        })
    })
}

/// Captures one window through the in-process Quartz path without spawning a helper.
///
/// This is intended for live human-facing previews. Model observations use
/// [`capture_window`] so they retain the more reliable screenshot-service fallback.
pub fn capture_window_in_process(window_id: u32) -> Result<NativeImage, CaptureWindowError> {
    capture_window_legacy(window_id).ok_or_else(|| CaptureWindowError {
        message: format!("window {window_id} is unavailable to in-process capture"),
    })
}

fn capture_window_service(window_id: u32) -> Result<NativeImage, CaptureWindowError> {
    let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nanocodex-capture-{}-{sequence}-{window_id}.png",
        std::process::id()
    ));
    let temporary = TemporaryCapture(path);
    let window_id = window_id.to_string();
    let mut child = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-o", "-l", &window_id, "-t", "png"])
        .arg(&temporary.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| CaptureWindowError {
            message: format!("could not launch /usr/sbin/screencapture: {error}"),
        })?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CaptureWindowError {
                    message: "capture exceeded its 3s deadline and was terminated".to_owned(),
                });
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CaptureWindowError {
                    message: format!("could not wait for screenshot service: {error}"),
                });
            }
        }
    };
    if !status.success() {
        return Err(CaptureWindowError {
            message: format!("screenshot service exited with {status}"),
        });
    }
    let png = std::fs::read(&temporary.0).map_err(|error| CaptureWindowError {
        message: format!("could not read screenshot service output: {error}"),
    })?;
    let (width, height) = png_dimensions(&png)?;
    Ok(NativeImage {
        data: NativeImageData::Png(png),
        width,
        height,
    })
}

struct TemporaryCapture(PathBuf);

impl Drop for TemporaryCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn png_dimensions(png: &[u8]) -> Result<(u32, u32), CaptureWindowError> {
    const PNG_HEADER: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || png.get(..8) != Some(PNG_HEADER) || png.get(12..16) != Some(b"IHDR") {
        return Err(CaptureWindowError {
            message: "screenshot service returned invalid PNG data".to_owned(),
        });
    }
    let width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| CaptureWindowError {
        message: "screenshot PNG has no width".to_owned(),
    })?);
    let height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| CaptureWindowError {
        message: "screenshot PNG has no height".to_owned(),
    })?);
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(CaptureWindowError {
            message: format!("screenshot service returned invalid dimensions {width}x{height}"),
        });
    }
    Ok((width, height))
}

fn capture_window_legacy(window_id: u32) -> Option<NativeImage> {
    let bounds = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
    let image = create_image(
        bounds,
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageBoundsIgnoreFraming | kCGWindowImageBestResolution,
    )?;
    render_image(&image)
}

fn render_image(image: &core_graphics::image::CGImage) -> Option<NativeImage> {
    let source_width = image.width();
    let source_height = image.height();
    if source_width == 0 || source_height == 0 || source_width > 16_384 || source_height > 16_384 {
        return None;
    }
    let source_pixels = source_width.checked_mul(source_height)?;
    let scale = if source_pixels > MAX_CAPTURE_PIXELS {
        (MAX_CAPTURE_PIXELS as f64 / source_pixels as f64).sqrt()
    } else {
        1.0
    };
    let width = (source_width as f64 * scale).floor().max(1.0) as usize;
    let height = (source_height as f64 * scale).floor().max(1.0) as usize;
    let bytes_per_row = width.checked_mul(4)?;
    let color_space = CGColorSpace::create_device_rgb();
    let mut context = CGContext::create_bitmap_context(
        None,
        width,
        height,
        8,
        bytes_per_row,
        &color_space,
        kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big,
    );
    let destination = CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(width as f64, height as f64),
    );
    context.draw_image(destination, image);
    let rgba = context.data().to_vec();
    Some(NativeImage {
        data: NativeImageData::Rgba(rgba),
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

fn dictionary_value(
    dictionary: &CFDictionary<CFString, CFType>,
    key: CFStringRef,
) -> Option<CFType> {
    // SAFETY: Quartz key constants are process-lifetime CFStrings.
    let key = unsafe { CFString::wrap_under_get_rule(key) };
    dictionary.find(&key).map(|value| (*value).clone())
}

fn number(dictionary: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<CFNumber> {
    dictionary_value(dictionary, key)?.downcast::<CFNumber>()
}

fn string(dictionary: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<String> {
    dictionary_value(dictionary, key)?
        .downcast::<CFString>()
        .map(|value| value.to_string())
}

fn boolean(dictionary: &CFDictionary<CFString, CFType>, key: CFStringRef) -> Option<bool> {
    dictionary_value(dictionary, key)?
        .downcast::<CFBoolean>()
        .map(bool::from)
}

fn running_application(pid: i32) -> (Option<String>, Option<String>) {
    let Some(application) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
    else {
        return (None, None);
    };
    (
        application.localizedName().map(|value| value.to_string()),
        application
            .bundleIdentifier()
            .map(|value| value.to_string()),
    )
}

/// Returns the process identifier of the application currently owning focus.
#[must_use]
pub fn frontmost_application_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|application| application.processIdentifier())
}

/// Returns whether the public workspace API identifies macOS loginwindow as
/// the foreground application. Nanocodex treats that state as locked and never
/// attempts to synthesize an unlock.
#[must_use]
pub fn screen_locked() -> bool {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|application| application.bundleIdentifier())
        .is_some_and(|bundle_id| bundle_id.to_string() == "com.apple.loginwindow")
}

/// A non-activating in-process floating renderer for live computer frames.
///
/// Construction and every method must run on the macOS main thread. The safe
/// wrapper enforces that condition with `MainThreadMarker` before touching
/// AppKit.
pub struct NativePipWindow {
    application: Retained<NSApplication>,
    panel: Retained<NSPanel>,
    image_view: Retained<NSImageView>,
    cursor_view: Retained<NSImageView>,
    agent_cursor_view: Retained<NSImageView>,
    cursor_source: CGEventSource,
    source_frame: Cell<Option<CGRect>>,
    agent_cursor: Cell<Option<AgentCursor>>,
    presentation_aspect: Cell<Option<f64>>,
}

#[derive(Clone, Copy)]
struct AgentCursor {
    x: f64,
    y: f64,
    pressed_until: Option<Instant>,
}

/// Failure to create or update the native PIP renderer.
#[derive(Debug)]
pub struct NativePipWindowError;

impl NativePipWindow {
    /// Creates a non-key floating panel that the first frame presents.
    pub fn new() -> Result<Self, NativePipWindowError> {
        let mtm = MainThreadMarker::new().ok_or(NativePipWindowError)?;
        let application = NSApplication::sharedApplication(mtm);
        let _ = application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let size = NSSize::new(420.0, 270.0);
        let visible = NSScreen::mainScreen(mtm)
            .map(|screen| screen.visibleFrame())
            .unwrap_or_else(|| NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1_440.0, 900.0)));
        let origin = NSPoint::new(
            visible.origin.x + visible.size.width - size.width - 24.0,
            visible.origin.y + visible.size.height - size.height - 24.0,
        );
        let frame = NSRect::new(origin, size);
        let style = NSWindowStyleMask::Resizable | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setTitle(&NSString::from_str("Nanocodex Computer"));
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setCanHide(false);
        panel.setMovableByWindowBackground(true);
        panel.setMinSize(NSSize::new(160.0, 100.0));
        panel.setLevel(NSFloatingWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        // SAFETY: The panel remains strongly retained by this wrapper, so AppKit
        // must not release it implicitly when the user closes it.
        unsafe { panel.setReleasedWhenClosed(false) };
        let image_view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), size),
        );
        image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        image_view.setWantsLayer(true);
        if let Some(layer) = image_view.layer() {
            layer.setCornerRadius(14.0);
            layer.setMasksToBounds(true);
        }
        image_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let cursor_view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(18.0, 24.0)),
        );
        cursor_view.setImage(Some(&NSCursor::arrowCursor().image()));
        cursor_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        cursor_view.setHidden(true);
        image_view.addSubview(&cursor_view);
        let agent_cursor_view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(22.0, 29.0)),
        );
        agent_cursor_view.setImage(Some(&NSCursor::arrowCursor().image()));
        agent_cursor_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        agent_cursor_view.setHidden(true);
        image_view.addSubview(&agent_cursor_view);
        panel.setContentView(Some(&image_view));
        let cursor_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|()| NativePipWindowError)?;
        application.updateWindows();
        Ok(Self {
            application,
            panel,
            image_view,
            cursor_view,
            agent_cursor_view,
            cursor_source,
            source_frame: Cell::new(None),
            agent_cursor: Cell::new(None),
            presentation_aspect: Cell::new(None),
        })
    }

    /// Updates the global source-window geometry used to place the cursor overlay.
    pub fn set_source_frame(&self, x: f64, y: f64, width: f64, height: f64) {
        self.source_frame.set(Some(CGRect::new(
            &CGPoint::new(x, y),
            &CGSize::new(width, height),
        )));
    }

    /// Updates the independent logical cursor used by background agent input.
    pub fn set_agent_cursor(&self, x: f64, y: f64, pressed: bool) {
        let now = Instant::now();
        let pressed_until = if pressed {
            Some(now + Duration::from_millis(180))
        } else {
            self.agent_cursor
                .get()
                .and_then(|cursor| cursor.pressed_until)
                .filter(|deadline| *deadline > now)
        };
        self.agent_cursor.set(Some(AgentCursor {
            x,
            y,
            pressed_until,
        }));
    }

    /// Replaces the displayed frame from complete PNG bytes.
    pub fn update_png(&self, png: &[u8]) -> Result<(), NativePipWindowError> {
        MainThreadMarker::new().ok_or(NativePipWindowError)?;
        // SAFETY: NSData copies exactly `png.len()` initialized bytes during the
        // call and does not retain the borrowed Rust pointer.
        let data =
            unsafe { NSData::dataWithBytes_length(png.as_ptr().cast::<c_void>(), png.len()) };
        let image = NSImage::initWithData(NSImage::alloc(), &data).ok_or(NativePipWindowError)?;
        self.present_image(&image)
    }

    /// Replaces the displayed frame from tightly packed premultiplied RGBA8 pixels.
    pub fn update_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), NativePipWindowError> {
        MainThreadMarker::new().ok_or(NativePipWindowError)?;
        let width = usize::try_from(width).map_err(|_| NativePipWindowError)?;
        let height = usize::try_from(height).map_err(|_| NativePipWindowError)?;
        let bytes_per_row = width.checked_mul(4).ok_or(NativePipWindowError)?;
        let expected = bytes_per_row
            .checked_mul(height)
            .ok_or(NativePipWindowError)?;
        if rgba.len() != expected {
            return Err(NativePipWindowError);
        }
        // SAFETY: A null planes pointer asks AppKit to allocate the bitmap.
        // The validated dimensions and row stride describe the complete RGBA8
        // buffer copied immediately below.
        let representation = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                width as isize,
                height as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                bytes_per_row as isize,
                32,
            )
        }
        .ok_or(NativePipWindowError)?;
        let bitmap = representation.bitmapData();
        if bitmap.is_null() {
            return Err(NativePipWindowError);
        }
        // SAFETY: AppKit allocated at least `bytes_per_row * height` bytes for
        // the representation and both slices are valid and non-overlapping.
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), bitmap, rgba.len()) };
        let image =
            NSImage::initWithSize(NSImage::alloc(), NSSize::new(width as f64, height as f64));
        image.addRepresentation(&representation);
        self.present_image(&image)
    }

    fn present_image(&self, image: &NSImage) -> Result<(), NativePipWindowError> {
        let image_size = image.size();
        if image_size.width > 0.0 && image_size.height > 0.0 {
            let aspect = image_size.width / image_size.height;
            let previous_aspect = self.presentation_aspect.get();
            if previous_aspect.is_none_or(|previous| (previous - aspect).abs() > 0.02) {
                let previous_frame = self.panel.frame();
                let size = if previous_aspect.is_none() {
                    let scale = (420.0 / image_size.width).min(270.0 / image_size.height);
                    NSSize::new(image_size.width * scale, image_size.height * scale)
                } else {
                    let area = previous_frame.size.width * previous_frame.size.height;
                    NSSize::new((area * aspect).sqrt(), (area / aspect).sqrt())
                };
                self.panel.setContentSize(size);
                let resized_frame = self.panel.frame();
                if previous_aspect.is_none() {
                    if let Some(screen) =
                        NSScreen::mainScreen(MainThreadMarker::new().ok_or(NativePipWindowError)?)
                    {
                        let visible = screen.visibleFrame();
                        self.panel.setFrameOrigin(NSPoint::new(
                            visible.origin.x + visible.size.width - resized_frame.size.width - 24.0,
                            visible.origin.y + visible.size.height
                                - resized_frame.size.height
                                - 24.0,
                        ));
                    }
                } else {
                    self.panel.setFrameOrigin(NSPoint::new(
                        previous_frame.origin.x + previous_frame.size.width
                            - resized_frame.size.width,
                        previous_frame.origin.y + previous_frame.size.height
                            - resized_frame.size.height,
                    ));
                }
                self.presentation_aspect.set(Some(aspect));
            }
            self.panel.setContentAspectRatio(image_size);
        }
        self.image_view.setImage(Some(image));
        self.panel.orderFrontRegardless();
        self.application.updateWindows();
        Ok(())
    }

    /// Drains a bounded number of pending AppKit events without blocking.
    pub fn pump(&self) -> Result<(), NativePipWindowError> {
        MainThreadMarker::new().ok_or(NativePipWindowError)?;
        self.update_cursor();
        self.update_agent_cursor();
        let expiration = NSDate::distantPast();
        for _ in 0..32 {
            let Some(event) = self
                .application
                .nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&expiration),
                    // SAFETY: NSDefaultRunLoopMode is a process-lifetime Foundation constant.
                    unsafe { NSDefaultRunLoopMode },
                    true,
                )
            else {
                break;
            };
            self.application.sendEvent(&event);
        }
        self.application.updateWindows();
        Ok(())
    }

    fn update_cursor(&self) {
        let Some(source) = self.source_frame.get() else {
            self.cursor_view.setHidden(true);
            return;
        };
        if source.size.width <= 0.0 || source.size.height <= 0.0 {
            self.cursor_view.setHidden(true);
            return;
        }
        let Ok(event) = CGEvent::new(self.cursor_source.clone()) else {
            self.cursor_view.setHidden(true);
            return;
        };
        let cursor = event.location();
        let relative_x = (cursor.x - source.origin.x) / source.size.width;
        let relative_y = (cursor.y - source.origin.y) / source.size.height;
        if !(0.0..=1.0).contains(&relative_x) || !(0.0..=1.0).contains(&relative_y) {
            self.cursor_view.setHidden(true);
            return;
        }
        let bounds = self.image_view.bounds();
        let cursor_size = self.cursor_view.frame().size;
        self.cursor_view.setFrameOrigin(NSPoint::new(
            bounds.origin.x + relative_x * bounds.size.width,
            bounds.origin.y + (1.0 - relative_y) * bounds.size.height - cursor_size.height,
        ));
        self.cursor_view.setHidden(false);
    }

    fn update_agent_cursor(&self) {
        let Some(cursor) = self.agent_cursor.get() else {
            self.agent_cursor_view.setHidden(true);
            return;
        };
        let Some(source) = self.source_frame.get() else {
            self.agent_cursor_view.setHidden(true);
            return;
        };
        if source.size.width <= 0.0 || source.size.height <= 0.0 {
            self.agent_cursor_view.setHidden(true);
            return;
        }
        let relative_x = (cursor.x - source.origin.x) / source.size.width;
        let relative_y = (cursor.y - source.origin.y) / source.size.height;
        if !(0.0..=1.0).contains(&relative_x) || !(0.0..=1.0).contains(&relative_y) {
            self.agent_cursor_view.setHidden(true);
            return;
        }
        let pressed = cursor
            .pressed_until
            .is_some_and(|deadline| deadline > Instant::now());
        let size = if pressed {
            NSSize::new(26.0, 34.0)
        } else {
            NSSize::new(22.0, 29.0)
        };
        self.agent_cursor_view.setFrameSize(size);
        let bounds = self.image_view.bounds();
        self.agent_cursor_view.setFrameOrigin(NSPoint::new(
            bounds.origin.x + relative_x * bounds.size.width,
            bounds.origin.y + (1.0 - relative_y) * bounds.size.height - size.height,
        ));
        self.agent_cursor_view.setHidden(false);
    }

    /// Removes the panel from screen without activating another application.
    pub fn hide(&self) -> Result<(), NativePipWindowError> {
        MainThreadMarker::new().ok_or(NativePipWindowError)?;
        self.panel.orderOut(None);
        Ok(())
    }
}

/// Requests activation of one running graphical application.
#[must_use]
pub fn activate_application(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_some_and(|application| {
        application.activateWithOptions(NSApplicationActivationOptions::empty())
    })
}

/// Monotonic Accessibility notification counters for one application process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccessibilitySignalSnapshot {
    pub revision: u64,
    pub tree_revision: u64,
    pub window_revision: u64,
    pub busy_revision: u64,
}

#[derive(Default)]
struct AccessibilitySignalState {
    revision: AtomicU64,
    tree_revision: AtomicU64,
    window_revision: AtomicU64,
    busy_revision: AtomicU64,
    wait_lock: Mutex<()>,
    changed: Condvar,
}

impl AccessibilitySignalState {
    fn record(&self, notification: &str) {
        let _guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.revision.fetch_add(1, Ordering::AcqRel);
        if [
            kAXFocusedWindowChangedNotification,
            kAXMainWindowChangedNotification,
            kAXWindowCreatedNotification,
        ]
        .contains(&notification)
        {
            self.window_revision.fetch_add(1, Ordering::AcqRel);
        } else if notification == kAXElementBusyChangedNotification {
            self.busy_revision.fetch_add(1, Ordering::AcqRel);
        } else {
            self.tree_revision.fetch_add(1, Ordering::AcqRel);
        }
        self.changed.notify_all();
    }

    fn snapshot(&self) -> AccessibilitySignalSnapshot {
        AccessibilitySignalSnapshot {
            revision: self.revision.load(Ordering::Acquire),
            tree_revision: self.tree_revision.load(Ordering::Acquire),
            window_revision: self.window_revision.load(Ordering::Acquire),
            busy_revision: self.busy_revision.load(Ordering::Acquire),
        }
    }

    fn wait_for_change(&self, revision: u64, timeout: Duration) -> bool {
        if self.revision.load(Ordering::Acquire) != revision {
            return true;
        }
        let guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.revision.load(Ordering::Acquire) != revision {
            return true;
        }
        let (_guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |_| {
                self.revision.load(Ordering::Acquire) == revision
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.revision.load(Ordering::Acquire) != revision
    }
}

/// Cloneable wait handle for Accessibility-driven observation invalidations.
#[derive(Clone)]
pub struct AccessibilitySignalWaiter {
    signals: Arc<AccessibilitySignalState>,
}

impl AccessibilitySignalWaiter {
    /// Blocks until a newer Accessibility revision is observed or the timeout expires.
    #[must_use]
    pub fn wait_for_change(&self, revision: u64, timeout: Duration) -> bool {
        self.signals.wait_for_change(revision, timeout)
    }
}

/// Lifecycle-owned AXObserver run-loop thread for one attached process.
pub struct AccessibilityNotificationMonitor {
    stopped: Arc<AtomicBool>,
    signals: Arc<AccessibilitySignalState>,
    thread: Option<JoinHandle<()>>,
}

/// Failure to create an Accessibility notification observer.
#[derive(Debug)]
pub struct AccessibilityNotificationMonitorError;

impl AccessibilityNotificationMonitor {
    /// Starts observing window, tree, value, focus, and loading invalidations.
    pub fn spawn(pid: i32) -> Result<Self, AccessibilityNotificationMonitorError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let signals = Arc::new(AccessibilitySignalState::default());
        let thread_stopped = Arc::clone(&stopped);
        let thread_signals = Arc::clone(&signals);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("nanocodex-ax-monitor".to_owned())
            .spawn(move || {
                let application = AXUIElement::application(pid);
                let mut raw_observer = std::ptr::null_mut();
                // SAFETY: The out pointer is valid for the call and the callback
                // retains no AX values. A successful create returns one owned
                // observer, wrapped below under Core Foundation's create rule.
                if unsafe {
                    AXObserverCreate(
                        pid,
                        accessibility_notification_callback,
                        &raw mut raw_observer,
                    )
                } != 0
                    || raw_observer.is_null()
                {
                    let _ = ready_tx.send(false);
                    return;
                }
                // SAFETY: AXObserverCreate succeeded and transferred one owned
                // retain to this thread.
                let observer =
                    unsafe { AccessibilityObserver::wrap_under_create_rule(raw_observer) };
                let notifications = [
                    kAXFocusedWindowChangedNotification,
                    kAXMainWindowChangedNotification,
                    kAXWindowCreatedNotification,
                    kAXFocusedUIElementChangedNotification,
                    kAXElementBusyChangedNotification,
                    kAXLayoutChangedNotification,
                    kAXCreatedNotification,
                    kAXUIElementDestroyedNotification,
                    kAXValueChangedNotification,
                    kAXTitleChangedNotification,
                    kAXMovedNotification,
                    kAXResizedNotification,
                    kAXRowCountChangedNotification,
                    kAXSelectedChildrenChangedNotification,
                    kAXSelectedTextChangedNotification,
                ];
                let refcon = Arc::as_ptr(&thread_signals).cast_mut().cast::<c_void>();
                let accepted = notifications
                    .iter()
                    .filter(|notification| {
                        let notification = CFString::new(notification);
                        // SAFETY: The observer, application element, notification
                        // string, and refcon remain valid for the run-loop lifetime.
                        unsafe {
                            AXObserverAddNotification(
                                observer.as_concrete_TypeRef(),
                                application.as_concrete_TypeRef(),
                                notification.as_concrete_TypeRef(),
                                refcon,
                            ) == 0
                        }
                    })
                    .count();
                if accepted == 0 {
                    let _ = ready_tx.send(false);
                    return;
                }
                // SAFETY: The observer owns a valid run-loop source. Wrapping
                // under the get rule retains it independently for this scope.
                let source = unsafe {
                    CFRunLoopSource::wrap_under_get_rule(AXObserverGetRunLoopSource(
                        observer.as_concrete_TypeRef(),
                    ))
                };
                let run_loop = CFRunLoop::get_current();
                // SAFETY: kCFRunLoopDefaultMode is a process-lifetime constant.
                run_loop.add_source(&source, unsafe { kCFRunLoopDefaultMode });
                let _ = ready_tx.send(true);
                while !thread_stopped.load(Ordering::Acquire) {
                    // SAFETY: kCFRunLoopDefaultMode is a process-lifetime constant.
                    CFRunLoop::run_in_mode(
                        unsafe { kCFRunLoopDefaultMode },
                        Duration::from_millis(100),
                        false,
                    );
                }
            })
            .map_err(|_| AccessibilityNotificationMonitorError)?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => Ok(Self {
                stopped,
                signals,
                thread: Some(thread),
            }),
            _ => {
                stopped.store(true, Ordering::Release);
                let _ = thread.join();
                Err(AccessibilityNotificationMonitorError)
            }
        }
    }

    /// Returns the latest lock-free notification counters.
    #[must_use]
    pub fn snapshot(&self) -> AccessibilitySignalSnapshot {
        self.signals.snapshot()
    }

    /// Returns a cloneable handle that can wake a blocking settle wait.
    #[must_use]
    pub fn waiter(&self) -> AccessibilitySignalWaiter {
        AccessibilitySignalWaiter {
            signals: Arc::clone(&self.signals),
        }
    }
}

impl Drop for AccessibilityNotificationMonitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "C" fn accessibility_notification_callback(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
) {
    if notification.is_null() || refcon.is_null() {
        return;
    }
    // SAFETY: spawn passes an Arc-backed state pointer that outlives the
    // observer run loop, and the callback only performs atomic operations.
    let signals = unsafe { &*refcon.cast::<AccessibilitySignalState>() };
    // SAFETY: Accessibility supplies a valid borrowed CFString for this call.
    let notification = unsafe { CFString::wrap_under_get_rule(notification) };
    signals.record(&notification.to_string());
}

/// A supervised listen-only human input event tap.
pub struct HumanInputMonitor {
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Failure to create the listen-only event tap or its worker thread.
#[derive(Debug)]
pub struct HumanInputMonitorError;

/// Structural metadata for a physical input event directed at the target app.
#[derive(Clone, Copy, Debug)]
pub struct HumanInputEvent {
    pub kind: &'static str,
    pub source_pid: i64,
    pub target_pid: i32,
    pub x: f64,
    pub y: f64,
}

/// Lock-free target identity shared with the listen-only input monitor.
#[derive(Default)]
pub struct HumanInputTarget {
    pid: AtomicI32,
    synthetic_pointer_events: Mutex<VecDeque<SyntheticPointerEvent>>,
    synthetic_pointer_fallbacks: AtomicU64,
}

struct SyntheticPointerEvent {
    kind: u32,
    location: CGPoint,
    expires_at: Instant,
}

impl HumanInputTarget {
    /// Replaces the process currently owned by the computer actor.
    pub fn set_pid(&self, pid: i32) {
        self.pid.store(pid, Ordering::Release);
    }

    fn pid(&self) -> i32 {
        self.pid.load(Ordering::Acquire)
    }

    /// Records a generated pointer event in case macOS strips its user-data marker.
    pub fn expect_synthetic_pointer_event(&self, event: &CGEvent) {
        let kind = event.get_type();
        if !matches!(
            kind,
            CGEventType::LeftMouseDown
                | CGEventType::RightMouseDown
                | CGEventType::OtherMouseDown
                | CGEventType::ScrollWheel
        ) {
            return;
        }
        let now = Instant::now();
        let mut expected = self
            .synthetic_pointer_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expected.retain(|event| event.expires_at > now);
        if expected.len() == 64 {
            expected.pop_front();
        }
        expected.push_back(SyntheticPointerEvent {
            kind: kind as u32,
            location: event.location(),
            expires_at: now + Duration::from_millis(500),
        });
    }

    /// Returns how many generated pointer events required expectation matching.
    #[must_use]
    pub fn synthetic_pointer_fallback_count(&self) -> u64 {
        self.synthetic_pointer_fallbacks.load(Ordering::Acquire)
    }

    fn consume_synthetic_pointer_event(&self, kind: CGEventType, event: &CGEvent) -> bool {
        let now = Instant::now();
        let location = event.location();
        let mut expected = self
            .synthetic_pointer_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expected.retain(|event| event.expires_at > now);
        let Some(index) = expected.iter().position(|expected| {
            expected.kind == kind as u32
                && (kind as u32 == CGEventType::ScrollWheel as u32
                    || ((expected.location.x - location.x).abs() <= 1.0
                        && (expected.location.y - location.y).abs() <= 1.0))
        }) else {
            return false;
        };
        expected.remove(index);
        self.synthetic_pointer_fallbacks
            .fetch_add(1, Ordering::AcqRel);
        true
    }
}

impl HumanInputMonitor {
    /// Starts observing physical input directed at the selected application.
    pub fn spawn(
        target: Arc<HumanInputTarget>,
        callback: impl Fn(HumanInputEvent) + Send + Sync + 'static,
    ) -> Result<Self, HumanInputMonitorError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let callback: Arc<dyn Fn(HumanInputEvent) + Send + Sync> = Arc::new(callback);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("nanocodex-input-monitor".to_owned())
            .spawn(move || {
                let callback = Arc::clone(&callback);
                let result = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::TailAppendEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![
                        CGEventType::LeftMouseDown,
                        CGEventType::RightMouseDown,
                        CGEventType::OtherMouseDown,
                        CGEventType::ScrollWheel,
                        CGEventType::KeyDown,
                    ],
                    move |_proxy, kind, event| {
                        let target_pid = target.pid();
                        let marker =
                            event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA);
                        if marker != SYNTHETIC_MARKER
                            && !target.consume_synthetic_pointer_event(kind, event)
                            && human_event_targets(kind, event, target_pid)
                        {
                            let location = event.location();
                            callback(HumanInputEvent {
                                kind: input_event_kind(kind),
                                source_pid: event.get_integer_value_field(
                                    EventField::EVENT_SOURCE_UNIX_PROCESS_ID,
                                ),
                                target_pid,
                                x: location.x,
                                y: location.y,
                            });
                        }
                        CallbackResult::Keep
                    },
                );
                let Ok(tap) = result else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let Ok(source) = tap.mach_port().create_runloop_source(0) else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let run_loop = CFRunLoop::get_current();
                // SAFETY: kCFRunLoopDefaultMode is a process-lifetime CoreFoundation constant.
                run_loop.add_source(&source, unsafe { kCFRunLoopDefaultMode });
                tap.enable();
                let _ = ready_tx.send(true);
                while !thread_stopped.load(Ordering::Acquire) {
                    // SAFETY: kCFRunLoopDefaultMode is a process-lifetime CoreFoundation constant.
                    CFRunLoop::run_in_mode(
                        unsafe { kCFRunLoopDefaultMode },
                        Duration::from_millis(100),
                        false,
                    );
                }
            })
            .map_err(|_| HumanInputMonitorError)?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => Ok(Self {
                stopped,
                thread: Some(thread),
            }),
            _ => {
                stopped.store(true, Ordering::Release);
                let _ = thread.join();
                Err(HumanInputMonitorError)
            }
        }
    }
}

const fn input_event_kind(kind: CGEventType) -> &'static str {
    match kind {
        CGEventType::KeyDown => "key_down",
        CGEventType::LeftMouseDown => "left_mouse_down",
        CGEventType::RightMouseDown => "right_mouse_down",
        CGEventType::OtherMouseDown => "other_mouse_down",
        CGEventType::ScrollWheel => "scroll_wheel",
        _ => "other",
    }
}

fn human_event_targets(kind: CGEventType, event: &CGEvent, target_pid: i32) -> bool {
    if target_pid <= 0 {
        return false;
    }
    match kind {
        CGEventType::KeyDown => frontmost_application_pid() == Some(target_pid),
        CGEventType::LeftMouseDown
        | CGEventType::RightMouseDown
        | CGEventType::OtherMouseDown
        | CGEventType::ScrollWheel => topmost_window_pid(event.location()) == Some(target_pid),
        _ => false,
    }
}

fn topmost_window_pid(point: CGPoint) -> Option<i32> {
    let raw = copy_window_info(
        kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    let reference = raw.as_concrete_TypeRef();
    std::mem::forget(raw);
    // SAFETY: Quartz documents every element returned by
    // CGWindowListCopyWindowInfo as CFDictionary<CFString, CFType>, ordered
    // front to back. Unlike target discovery, this deliberately retains
    // floating layers so clicks on the PIP do not fall through to its source.
    let dictionaries =
        unsafe { CFArray::<CFDictionary<CFString, CFType>>::wrap_under_create_rule(reference) };
    dictionaries.iter().find_map(|dictionary| {
        if !boolean(&dictionary, unsafe { kCGWindowIsOnscreen }).unwrap_or(false) {
            return None;
        }
        let bounds = dictionary_value(&dictionary, unsafe { kCGWindowBounds })?
            .downcast::<CFDictionary>()?;
        let frame = CGRect::from_dict_representation(&bounds)?;
        (frame.size.width > 0.0
            && frame.size.height > 0.0
            && point.x >= frame.origin.x
            && point.x < frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y < frame.origin.y + frame.size.height)
            .then(|| number(&dictionary, unsafe { kCGWindowOwnerPID })?.to_i32())
            .flatten()
    })
}

impl Drop for HumanInputMonitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn value_attribute<T: Copy>(
    element: &AXUIElement,
    name: &str,
    expected_type: u32,
    mut value: T,
) -> Option<T> {
    let attribute = AXAttribute::<CFType>::new(&CFString::new(name));
    let raw = element.attribute(&attribute).ok()?;
    if raw.type_of() != unsafe { AXValueGetTypeID() } {
        return None;
    }
    let raw_value = raw.as_CFTypeRef().cast::<accessibility_sys::__AXValue>() as AXValueRef;
    // SAFETY: The type ID check above proves `raw_value` is an AXValue. The
    // requested AXValue type is checked before writing into a correctly sized,
    // aligned initialized `T` matching that public CoreGraphics shape.
    unsafe {
        if AXValueGetType(raw_value) != expected_type
            || !AXValueGetValue(raw_value, expected_type, (&raw mut value).cast::<c_void>())
        {
            return None;
        }
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_png_dimensions_without_decoding_pixels() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\0\0\0\0\0".to_vec();
        png[16..20].copy_from_slice(&1_440_u32.to_be_bytes());
        png[20..24].copy_from_slice(&900_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png).unwrap(), (1_440, 900));
        assert!(png_dimensions(b"not a PNG").is_err());
    }
}
