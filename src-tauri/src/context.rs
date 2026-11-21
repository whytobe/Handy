use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;

/// Metadata describing a desktop application for context-aware transforms.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
}

/// Captured desktop context at the moment a transform shortcut is triggered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransformContext {
    pub app: Option<AppInfo>,
    pub selected_text: Option<String>,
    pub pid: Option<i32>,
}

/// Returns metadata and PID for the currently active foreground application.
pub fn get_frontmost_app() -> Option<(AppInfo, i32)> {
    #[cfg(target_os = "macos")]
    {
        macos::get_frontmost_app()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Lists user-facing running applications (regular activation policy),
/// excluding Handy itself, deduplicated by bundle identifier and sorted by name.
pub fn list_running_applications() -> Vec<AppInfo> {
    #[cfg(target_os = "macos")]
    {
        macos::list_running_applications()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Captures the foreground app and (when requested) currently selected text.
pub fn capture_transform_context(
    app: &AppHandle,
    capture_selection: bool,
    allow_clipboard_fallback: bool,
) -> TransformContext {
    let frontmost = get_frontmost_app();
    let (app_info, pid) = match frontmost {
        Some((info, pid)) => (Some(info), Some(pid)),
        None => (None, None),
    };

    if !capture_selection {
        return TransformContext {
            app: app_info,
            selected_text: None,
            pid,
        };
    }

    #[cfg(target_os = "macos")]
    let selected_text = macos::capture_selected_text(app, pid, allow_clipboard_fallback);

    #[cfg(not(target_os = "macos"))]
    let selected_text = {
        let _ = (app, allow_clipboard_fallback);
        log::debug!("Selected text capture is currently only supported on macOS");
        None
    };

    log::debug!(
        "Transform context captured: app={:?} ({:?}), selected_chars={:?}, pid={:?}",
        app_info.as_ref().map(|a| a.name.as_str()),
        app_info.as_ref().map(|a| a.bundle_id.as_str()),
        selected_text.as_ref().map(|s| s.chars().count()),
        pid
    );

    TransformContext {
        app: app_info,
        selected_text,
        pid,
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::AppInfo;
    use log::debug;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
    use std::collections::HashSet;
    use std::ffi::{c_void, CStr, CString};
    use tauri::AppHandle;

    type CFTypeRef = *const c_void;
    type AXUIElementRef = *const c_void;
    type AXError = i32;

    const K_AX_ERROR_SUCCESS: AXError = 0;
    const K_AX_VALUE_TYPE_CF_RANGE: u32 = 4;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct CFRange {
        location: isize,
        length: isize,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFTypeRef,
            value: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementCopyParameterizedAttributeValue(
            element: AXUIElementRef,
            parameterized_attribute: CFTypeRef,
            parameter: CFTypeRef,
            result: *mut CFTypeRef,
        ) -> AXError;
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFTypeRef,
            value: CFTypeRef,
        ) -> AXError;
        fn AXValueGetValue(value: CFTypeRef, value_type: u32, value_ptr: *mut c_void) -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFStringCreateWithCString(
            alloc: CFTypeRef,
            c_str: *const i8,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFStringGetLength(the_string: CFTypeRef) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCStringPtr(the_string: CFTypeRef, encoding: u32) -> *const i8;
        fn CFStringGetCString(
            the_string: CFTypeRef,
            buffer: *mut u8,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFStringCreateWithSubstring(
            alloc: CFTypeRef,
            str_ref: CFTypeRef,
            range: CFRange,
        ) -> CFTypeRef;
    }

    /// RAII wrapper for CoreFoundation / Accessibility retained references.
    struct CfGuard(CFTypeRef);

    impl CfGuard {
        fn is_null(&self) -> bool {
            self.0.is_null()
        }
    }

    impl Drop for CfGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn make_cf_string(s: &str) -> Option<CfGuard> {
        let c_str = CString::new(s).ok()?;
        let cf = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c_str.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        if cf.is_null() {
            None
        } else {
            Some(CfGuard(cf))
        }
    }

    unsafe fn cf_string_to_rust(cf: CFTypeRef) -> Option<String> {
        if cf.is_null() || CFGetTypeID(cf) != CFStringGetTypeID() {
            return None;
        }
        let fast = CFStringGetCStringPtr(cf, K_CF_STRING_ENCODING_UTF8);
        if !fast.is_null() {
            return Some(CStr::from_ptr(fast).to_string_lossy().into_owned());
        }

        let len = CFStringGetLength(cf);
        let max_size = CFStringGetMaximumSizeForEncoding(len, K_CF_STRING_ENCODING_UTF8);
        if max_size < 0 {
            return None;
        }
        let buf_len = (max_size as usize).saturating_add(1);
        let mut buf = vec![0u8; buf_len];
        if CFStringGetCString(
            cf,
            buf.as_mut_ptr(),
            buf_len as isize,
            K_CF_STRING_ENCODING_UTF8,
        ) != 0
        {
            let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            Some(String::from_utf8_lossy(&buf[..nul_pos]).into_owned())
        } else {
            None
        }
    }

    fn copy_ax_attribute(element: AXUIElementRef, attr_name: &str) -> Option<CfGuard> {
        let attr = make_cf_string(attr_name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe { AXUIElementCopyAttributeValue(element, attr.0, &mut value) };
        if err == K_AX_ERROR_SUCCESS && !value.is_null() {
            Some(CfGuard(value))
        } else {
            None
        }
    }

    fn copy_ax_parameterized_attribute(
        element: AXUIElementRef,
        attr_name: &str,
        parameter: CFTypeRef,
    ) -> Option<CfGuard> {
        let attr = make_cf_string(attr_name)?;
        let mut value: CFTypeRef = std::ptr::null();
        let err = unsafe {
            AXUIElementCopyParameterizedAttributeValue(element, attr.0, parameter, &mut value)
        };
        if err == K_AX_ERROR_SUCCESS && !value.is_null() {
            Some(CfGuard(value))
        } else {
            None
        }
    }

    fn copy_ax_string(element: AXUIElementRef, attr_name: &str) -> Option<String> {
        let val = copy_ax_attribute(element, attr_name)?;
        unsafe { cf_string_to_rust(val.0) }
    }

    fn read_selected_text_marker_range(focused: AXUIElementRef) -> Option<String> {
        let marker_range = copy_ax_attribute(focused, "AXSelectedTextMarkerRange")?;
        let str_cf =
            copy_ax_parameterized_attribute(focused, "AXStringForTextMarkerRange", marker_range.0)?;
        let text = unsafe { cf_string_to_rust(str_cf.0) }?;
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn read_selected_text_from_element(focused: AXUIElementRef) -> Option<String> {
        if let Some(selected) = copy_ax_string(focused, "AXSelectedText") {
            if !selected.trim().is_empty() {
                return Some(selected);
            }
        }

        // Tier 1a2: WebKit, Safari, Chromium, and Electron text areas often expose
        // AXSelectedTextMarkerRange + AXStringForTextMarkerRange instead of AXSelectedText.
        if let Some(selected) = read_selected_text_marker_range(focused) {
            return Some(selected);
        }

        // Tier 1b: some native views expose AXSelectedTextRange + AXValue
        // without implementing AXSelectedText directly.
        let range_val = copy_ax_attribute(focused, "AXSelectedTextRange")?;
        let mut range = CFRange::default();
        let ok = unsafe {
            AXValueGetValue(
                range_val.0,
                K_AX_VALUE_TYPE_CF_RANGE,
                (&mut range as *mut CFRange).cast::<c_void>(),
            )
        };
        if ok == 0 || range.location < 0 || range.length <= 0 {
            return None;
        }

        let full_val = copy_ax_attribute(focused, "AXValue")?;
        unsafe {
            if CFGetTypeID(full_val.0) != CFStringGetTypeID() {
                return None;
            }
            let total_len = CFStringGetLength(full_val.0);
            let end = range.location.checked_add(range.length)?;
            if end > total_len {
                return None;
            }
            let substr = CfGuard(CFStringCreateWithSubstring(
                std::ptr::null(),
                full_val.0,
                range,
            ));
            if substr.is_null() {
                return None;
            }
            cf_string_to_rust(substr.0).filter(|s| !s.trim().is_empty())
        }
    }

    fn focused_element_for_pid(pid: Option<i32>) -> Option<CfGuard> {
        let system = CfGuard(unsafe { AXUIElementCreateSystemWide() });
        if !system.is_null() {
            if let Some(focused) = copy_ax_attribute(system.0, "AXFocusedUIElement") {
                return Some(focused);
            }
        }
        if let Some(pid) = pid {
            let app_el = CfGuard(unsafe { AXUIElementCreateApplication(pid) });
            if !app_el.is_null() {
                return copy_ax_attribute(app_el.0, "AXFocusedUIElement");
            }
        }
        None
    }

    fn capture_selected_text_via_ax(pid: Option<i32>) -> Option<String> {
        if unsafe { AXIsProcessTrusted() } == 0 {
            debug!("Accessibility not trusted; skipping AX selected text capture");
            return None;
        }

        if let Some(focused) = focused_element_for_pid(pid) {
            if let Some(text) = read_selected_text_from_element(focused.0) {
                return Some(text);
            }
        }

        // Electron / Chromium apps often keep their accessibility tree dormant
        // until AXManualAccessibility / AXEnhancedUserInterface is enabled.
        if let Some(pid) = pid {
            let app_el = CfGuard(unsafe { AXUIElementCreateApplication(pid) });
            if !app_el.is_null() {
                for attr_name in ["AXManualAccessibility", "AXEnhancedUserInterface"] {
                    if let Some(attr) = make_cf_string(attr_name) {
                        let _ = unsafe {
                            AXUIElementSetAttributeValue(app_el.0, attr.0, kCFBooleanTrue)
                        };
                    }
                }
                // Give Chromium/Electron a brief moment to populate the focused AX node.
                std::thread::sleep(std::time::Duration::from_millis(15));
                if let Some(focused) = copy_ax_attribute(app_el.0, "AXFocusedUIElement")
                    .or_else(|| focused_element_for_pid(Some(pid)))
                {
                    if let Some(text) = read_selected_text_from_element(focused.0) {
                        return Some(text);
                    }
                }
            }
        }

        None
    }

    pub(super) fn capture_selected_text(
        app: &AppHandle,
        pid: Option<i32>,
        allow_clipboard_fallback: bool,
    ) -> Option<String> {
        if let Some(text) = capture_selected_text_via_ax(pid) {
            debug!(
                "Captured selected text via macOS Accessibility ({} chars)",
                text.len()
            );
            return Some(text);
        }

        if allow_clipboard_fallback {
            if let Some(text) = crate::clipboard::copy_selection_via_clipboard(app) {
                debug!(
                    "Captured selected text via Cmd+C clipboard fallback ({} chars)",
                    text.len()
                );
                return Some(text);
            }
        }

        None
    }

    fn running_app_to_info(app: &NSRunningApplication) -> Option<AppInfo> {
        let name = app
            .localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        let bundle_id = app
            .bundleIdentifier()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() && bundle_id.is_empty() {
            return None;
        }
        Some(AppInfo {
            name: if name.is_empty() {
                bundle_id.clone()
            } else {
                name
            },
            bundle_id,
        })
    }

    pub(super) fn get_frontmost_app() -> Option<(AppInfo, i32)> {
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let front = workspace.frontmostApplication()?;
            let pid = front.processIdentifier();
            let info = running_app_to_info(&front)?;
            Some((info, pid))
        })
    }

    pub(super) fn list_running_applications() -> Vec<AppInfo> {
        autoreleasepool(|_| {
            let current_pid = std::process::id() as i32;
            let workspace = NSWorkspace::sharedWorkspace();
            let apps = workspace.runningApplications();
            let mut seen_bundles = HashSet::new();
            let mut result = Vec::new();

            for app in apps.iter() {
                if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                    continue;
                }
                if app.processIdentifier() == current_pid {
                    continue;
                }
                let Some(info) = running_app_to_info(&app) else {
                    continue;
                };
                let dedup_key = if info.bundle_id.is_empty() {
                    info.name.to_lowercase()
                } else {
                    info.bundle_id.to_lowercase()
                };
                if seen_bundles.insert(dedup_key) {
                    result.push(info);
                }
            }

            result.sort_by_key(|a| a.name.to_lowercase());
            result
        })
    }
}
