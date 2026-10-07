//! WebKitGTK rendering workarounds for the Linux AppImage.
//!
//! The AppImage bundles WebKitGTK (and GTK/GLib) from the Ubuntu build image but
//! uses the host's Mesa/EGL drivers. On rolling distros (Arch, CachyOS, Fedora)
//! the bundled WebKit's DMA-BUF renderer can't cope with the newer graphics
//! stack and crashes (SIGSEGV/abort) or renders a blank window the first time a
//! webview is shown, e.g. when "Open Dashboard" is clicked in the tray.
//!
//! Disabling the DMA-BUF renderer is the workaround Tauri documents for this
//! class of failure. It is scoped to AppImage runs, so deb/rpm installs that use
//! the system WebKitGTK keep the faster path, and it never overrides a value the
//! user set themselves (setting it to `0` opts back in).

use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};

const DMABUF_VAR: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

static APPLIED: AtomicBool = AtomicBool::new(false);

/// Whether the DMA-BUF renderer should be disabled, given the values of
/// `APPIMAGE` and `WEBKIT_DISABLE_DMABUF_RENDERER`.
fn should_disable_dmabuf(appimage: Option<&OsStr>, current: Option<&OsStr>) -> bool {
    let in_appimage = appimage.is_some_and(|p| !p.is_empty());
    in_appimage && current.is_none()
}

/// Apply the workaround. Must run at the very start of `main`, before any
/// threads are spawned and before GTK/WebKit are initialized.
pub fn apply() {
    if !cfg!(target_os = "linux") {
        return;
    }
    // var_os: an AppImage path (or user value) need not be valid UTF-8.
    let appimage = std::env::var_os("APPIMAGE");
    let current = std::env::var_os(DMABUF_VAR);
    if should_disable_dmabuf(appimage.as_deref(), current.as_deref()) {
        std::env::set_var(DMABUF_VAR, "1");
        APPLIED.store(true, Ordering::Relaxed);
    }
}

/// Log what `apply` did. Called once logging is initialized.
pub fn log_applied() {
    if APPLIED.load(Ordering::Relaxed) {
        log::info!(
            "Running from an AppImage: set {DMABUF_VAR}=1 to avoid WebKitGTK crashes with the host graphics stack (set {DMABUF_VAR}=0 to opt out)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::should_disable_dmabuf;
    use std::ffi::OsStr;

    fn s(v: &str) -> Option<&OsStr> {
        Some(OsStr::new(v))
    }

    #[test]
    fn disables_in_appimage_when_unset() {
        assert!(should_disable_dmabuf(s("/home/u/aw.AppImage"), None));
    }

    #[test]
    fn leaves_native_installs_alone() {
        assert!(!should_disable_dmabuf(None, None));
        assert!(!should_disable_dmabuf(s(""), None));
    }

    #[test]
    fn respects_user_value() {
        assert!(!should_disable_dmabuf(s("/a.AppImage"), s("0")));
        assert!(!should_disable_dmabuf(s("/a.AppImage"), s("1")));
    }

    #[cfg(unix)]
    #[test]
    fn handles_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let path = OsStr::from_bytes(b"/home/u/\xff/aw.AppImage");
        assert!(should_disable_dmabuf(Some(path), None));
        assert!(!should_disable_dmabuf(
            Some(path),
            Some(OsStr::from_bytes(b"\xff"))
        ));
    }
}
