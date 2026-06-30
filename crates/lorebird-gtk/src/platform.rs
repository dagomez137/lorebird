//! Platform-specific integration.
//!
//! On macOS a plain Cargo-built binary has no `.app` bundle, so the Dock
//! shows a generic executable icon. Setting the application icon at runtime
//! (`-[NSApplication setApplicationIconImage:]`) fixes the Dock/⌘-Tab icon
//! whether the app is launched via `cargo run` or from a bundle.

/// The lorebird logo, embedded so the Dock icon needs no external files.
///
/// Uses the macOS-compliant rounded-rectangle ("squircle") artwork — macOS
/// does not mask app icons, so the shape must be baked into the image.
#[cfg(target_os = "macos")]
const LOGO_PNG: &[u8] = include_bytes!("../resources/macos/lorebird-macos-512.png");

/// Set the macOS Dock / application-switcher icon to the lorebird logo.
///
/// Must be called on the main thread once the NSApplication exists (e.g.
/// from the GTK `activate` handler). A no-op on other platforms.
#[cfg(target_os = "macos")]
pub fn set_app_icon() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use std::ffi::c_void;

    unsafe {
        // NSData *data = [NSData dataWithBytes:LOGO_PNG length:LOGO_PNG.len()]
        let data: *mut AnyObject = msg_send![
            class!(NSData),
            dataWithBytes: LOGO_PNG.as_ptr() as *const c_void,
            length: LOGO_PNG.len(),
        ];
        if data.is_null() {
            return;
        }

        // NSImage *image = [[NSImage alloc] initWithData:data]
        let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![image, initWithData: data];
        if image.is_null() {
            return;
        }

        // [[NSApplication sharedApplication] setApplicationIconImage:image]
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_app_icon() {}
