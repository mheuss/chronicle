//! App metadata extraction via macOS APIs.
//!
//! Queries the system for the currently focused application's name, bundle
//! identifier, and window title. All fields are best-effort — a missing
//! value is `None`, never an error.

/// Metadata about the currently focused application.
#[derive(Debug, Clone, Default)]
pub struct AppMetadata {
    /// Display name of the frontmost app (e.g., "Safari").
    pub app_name: Option<String>,
    /// Bundle identifier (e.g., "com.apple.Safari").
    pub app_bundle_id: Option<String>,
    /// Title of the focused window (e.g., "Google - Safari").
    pub window_title: Option<String>,
}

/// Query macOS for the currently focused application and its window title.
///
/// Never fails. If any macOS API call returns nil or errors, the
/// corresponding field is `None`.
pub fn get_frontmost_app() -> AppMetadata {
    let (app_name, app_bundle_id, pid) = get_frontmost_app_info();
    let window_title = pid.and_then(get_window_title_for_pid);

    AppMetadata {
        app_name,
        app_bundle_id,
        window_title,
    }
}

/// Query NSWorkspace for the frontmost application.
/// Returns (app_name, bundle_id, pid).
fn get_frontmost_app_info() -> (Option<String>, Option<String>, Option<i32>) {
    use objc2_app_kit::NSWorkspace;

    // objc2-app-kit 0.3 marks these methods safe — no `unsafe` needed.
    let workspace = NSWorkspace::sharedWorkspace();
    let app = match workspace.frontmostApplication() {
        Some(app) => app,
        None => return (None, None, None),
    };

    let name = app.localizedName().map(|s| s.to_string());
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    let pid = app.processIdentifier();

    (name, bundle_id, Some(pid))
}

/// Get the title of the topmost normal-layer window for a given PID.
///
/// Uses CGWindowListCopyWindowInfo to enumerate on-screen windows.
/// Returns None if the app has no windows or window titles aren't
/// accessible (Screen Recording permission required for window names).
fn get_window_title_for_pid(target_pid: i32) -> Option<String> {
    use core_foundation::array::CFArray;
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::display::{
        kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    };

    // CGWindowListCopyWindowInfo FFI — core_graphics::display may not expose
    // a safe wrapper that returns the right type. Use the raw function.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(
            option: u32,
            relativeToWindow: u32,
        ) -> *const c_void;
    }
    use std::ffi::c_void;

    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;

    let window_list = unsafe { CGWindowListCopyWindowInfo(options, 0) };
    if window_list.is_null() {
        return None;
    }

    let cf_array: CFArray = unsafe { TCFType::wrap_under_create_rule(window_list as *const _) };

    let key_owner_pid = CFString::new("kCGWindowOwnerPID");
    let key_layer = CFString::new("kCGWindowLayer");
    let key_name = CFString::new("kCGWindowName");

    for i in 0..cf_array.len() {
        // Each element is a CFDictionary; get it as a raw pointer.
        let dict_ptr = unsafe {
            core_foundation::array::CFArrayGetValueAtIndex(
                cf_array.as_concrete_TypeRef(),
                i as isize,
            )
        };
        if dict_ptr.is_null() {
            continue;
        }
        let dict: CFDictionary =
            unsafe { TCFType::wrap_under_get_rule(dict_ptr as *const _) };

        // Check PID matches.
        let pid = dict
            .find(key_owner_pid.as_concrete_TypeRef() as *const c_void)
            .and_then(|v| unsafe {
                let num: CFNumber = TCFType::wrap_under_get_rule(*v as *const _);
                num.to_i32()
            });
        if pid != Some(target_pid) {
            continue;
        }

        // Only normal window layer (layer 0).
        let layer = dict
            .find(key_layer.as_concrete_TypeRef() as *const c_void)
            .and_then(|v| unsafe {
                let num: CFNumber = TCFType::wrap_under_get_rule(*v as *const _);
                num.to_i32()
            });
        if layer != Some(0) {
            continue;
        }

        // Get window name.
        if let Some(name_ptr) = dict.find(key_name.as_concrete_TypeRef() as *const c_void) {
            let name: CFString = unsafe { TCFType::wrap_under_get_rule(*name_ptr as *const _) };
            let title = name.to_string();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_frontmost_app_returns_metadata() {
        let meta = get_frontmost_app();
        assert!(
            meta.app_name.is_some(),
            "expected app_name to be Some, got None"
        );
    }

    #[test]
    fn get_frontmost_app_never_panics() {
        // Call 100 times rapidly — must never panic.
        for _ in 0..100 {
            let _ = get_frontmost_app();
        }
    }

    #[test]
    fn app_metadata_default_is_all_none() {
        let meta = AppMetadata::default();
        assert!(meta.app_name.is_none());
        assert!(meta.app_bundle_id.is_none());
        assert!(meta.window_title.is_none());
    }

    #[test]
    fn bundle_id_looks_like_reverse_dns() {
        let meta = get_frontmost_app();
        if let Some(ref bundle_id) = meta.app_bundle_id {
            assert!(
                bundle_id.contains('.'),
                "bundle_id '{bundle_id}' doesn't look like reverse-DNS"
            );
        }
    }

    #[test]
    fn app_metadata_debug_format() {
        let meta = get_frontmost_app();
        let debug_str = format!("{meta:?}");
        assert!(debug_str.contains("AppMetadata"));
    }
}
