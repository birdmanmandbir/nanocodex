//! Small audited bridge for APIs whose C value wrappers are missing from the
//! safe `accessibility` crate.

#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use accessibility::{AXAttribute, AXUIElement};
use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXValueGetType, AXValueGetTypeID,
    AXValueGetValue, AXValueRef, kAXPositionAttribute, kAXSizeAttribute,
    kAXTrustedCheckOptionPrompt, kAXValueTypeCGPoint, kAXValueTypeCGSize,
};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    runloop::{CFRunLoop, kCFRunLoopDefaultMode},
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
    geometry::{CGPoint, CGRect, CGSize},
    window::{
        copy_window_info, create_image, kCGNullWindowID, kCGWindowBounds,
        kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming, kCGWindowIsOnscreen,
        kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
        kCGWindowListOptionIncludingWindow, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
        kCGWindowOwnerPID,
    },
};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

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

/// Requests activation of one running graphical application.
#[must_use]
pub fn activate_application(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_some_and(|application| {
        application.activateWithOptions(NSApplicationActivationOptions::empty())
    })
}

/// A supervised listen-only human input event tap.
pub struct HumanInputMonitor {
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Failure to create the listen-only event tap or its worker thread.
#[derive(Debug)]
pub struct HumanInputMonitorError;

/// Lock-free target identity shared with the listen-only input monitor.
#[derive(Default)]
pub struct HumanInputTarget {
    pid: AtomicI32,
}

impl HumanInputTarget {
    /// Replaces the process currently owned by the computer actor.
    pub fn set_pid(&self, pid: i32) {
        self.pid.store(pid, Ordering::Release);
    }

    fn pid(&self) -> i32 {
        self.pid.load(Ordering::Acquire)
    }
}

impl HumanInputMonitor {
    /// Starts observing physical input directed at the selected application.
    pub fn spawn(
        target: Arc<HumanInputTarget>,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, HumanInputMonitorError> {
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
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
                        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                            != SYNTHETIC_MARKER
                            && human_event_targets(kind, event, target.pid())
                        {
                            callback();
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
    windows()
        .into_iter()
        .find(|window| {
            window.on_screen
                && window.width > 0.0
                && window.height > 0.0
                && point.x >= window.x
                && point.x < window.x + window.width
                && point.y >= window.y
                && point.y < window.y + window.height
        })
        .map(|window| window.pid)
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
