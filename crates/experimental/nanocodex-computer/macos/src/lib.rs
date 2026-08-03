//! Small audited bridge for APIs whose C value wrappers are missing from the
//! safe `accessibility` crate.

#![cfg(target_os = "macos")]

use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use accessibility::{AXAttribute, AXUIElement};
use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXValueGetType, AXValueGetTypeID,
    AXValueGetValue, AXValueRef, kAXPositionAttribute, kAXSizeAttribute,
    kAXTrustedCheckOptionPrompt, kAXValueTypeCGPoint, kAXValueTypeCGSize,
};
use block2::RcBlock;
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
use foreign_types::ForeignType;
use objc2::AnyThread;
use objc2_app_kit::NSRunningApplication;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

const SYNTHETIC_MARKER: i64 = 0x004e_414e_4f43_4458;
const MAX_CAPTURE_PIXELS: usize = 25_000_000;
static ACCESSIBILITY_REQUESTED: AtomicBool = AtomicBool::new(false);
static SCREEN_CAPTURE_REQUESTED: AtomicBool = AtomicBool::new(false);

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
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Enumerates all non-desktop layer-zero windows with their owning application.
#[must_use]
pub fn windows() -> Vec<NativeWindow> {
    let Some(raw) = copy_window_info(
        kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
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
        .filter_map(|dictionary| {
            let layer = number(&dictionary, unsafe { kCGWindowLayer })?.to_i32()?;
            if layer != 0 {
                return None;
            }
            let pid = number(&dictionary, unsafe { kCGWindowOwnerPID })?.to_i32()?;
            let id =
                u32::try_from(number(&dictionary, unsafe { kCGWindowNumber })?.to_i64()?).ok()?;
            let bounds = dictionary_value(&dictionary, unsafe { kCGWindowBounds })?;
            let bounds = bounds.downcast::<CFDictionary>()?;
            let frame = CGRect::from_dict_representation(&bounds)?;
            let owner_name = string(&dictionary, unsafe { kCGWindowOwnerName })?;
            let title =
                string(&dictionary, unsafe { kCGWindowName }).filter(|value| !value.is_empty());
            let on_screen = boolean(&dictionary, unsafe { kCGWindowIsOnscreen }).unwrap_or(false);
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
        })
        .collect()
}

/// Captures one window by ID without compositing other windows above it.
#[must_use]
pub fn capture_window(window_id: u32) -> Option<NativeImage> {
    capture_window_sck(window_id).or_else(|| capture_window_legacy(window_id))
}

fn capture_window_sck(window_id: u32) -> Option<NativeImage> {
    let (content_tx, content_rx) = std::sync::mpsc::sync_channel(1);
    let content_block = RcBlock::new(move |content: *mut SCShareableContent, _error| {
        if content.is_null() {
            let _ = content_tx.send(None);
            return;
        }
        // SAFETY: ScreenCaptureKit keeps the callback object alive for the
        // callback duration. `windows` returns its own retained NSArray.
        let windows = unsafe { (&*content).windows() };
        let selected = (0..windows.count())
            .map(|index| unsafe { windows.objectAtIndex_unchecked(index) })
            .find(|window| unsafe { window.windowID() == window_id });
        let Some(window) = selected else {
            let _ = content_tx.send(None);
            return;
        };
        // SAFETY: Both Objective-C initializers receive live retained objects.
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
        };
        let config = unsafe { SCStreamConfiguration::new() };
        let frame = unsafe { window.frame() };
        let width = frame.size.width.ceil().clamp(1.0, 16_384.0) as usize;
        let height = frame.size.height.ceil().clamp(1.0, 16_384.0) as usize;
        unsafe {
            config.setWidth(width);
            config.setHeight(height);
            config.setShowsCursor(true);
        }
        let image_tx = content_tx.clone();
        let image_block = RcBlock::new(move |image: *mut objc2_core_graphics::CGImage, _error| {
            if image.is_null() {
                let _ = image_tx.send(None);
                return;
            }
            // SAFETY: Retain the callback-borrowed CGImage, then transfer that
            // +1 reference to core-graphics' owning wrapper.
            let retained = unsafe {
                core_foundation::base::CFRetain(image.cast()) as *mut core_graphics::sys::CGImage
            };
            let image = unsafe { core_graphics::image::CGImage::from_ptr(retained) };
            let _ = image_tx.send(render_image(&image));
        });
        // SAFETY: ScreenCaptureKit retains the filter, configuration, and
        // completion block until the callback has completed.
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                &filter,
                &config,
                Some(&image_block),
            );
        }
    });
    // SAFETY: The copied block remains alive until ScreenCaptureKit invokes it.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &content_block,
        );
    }
    content_rx
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .flatten()
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
        rgba,
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

/// A supervised listen-only human input event tap.
pub struct HumanInputMonitor {
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Failure to create the listen-only event tap or its worker thread.
#[derive(Debug)]
pub struct HumanInputMonitorError;

impl HumanInputMonitor {
    /// Starts observing physical clicks, scrolls, and key presses.
    pub fn spawn(
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
                    move |_proxy, _kind, event| {
                        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                            != SYNTHETIC_MARKER
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
