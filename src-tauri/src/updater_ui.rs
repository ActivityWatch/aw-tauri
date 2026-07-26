//! Update progress dialog shown while downloading/installing an app update.
//!
//! The window loads a self-contained HTML page over a custom URI scheme
//! (`aw-update://`). A custom scheme (unlike a `data:` URL) has a valid Origin
//! so Tauri IPC works for "Restart Now" / "Later".
//! Progress is pushed from Rust with `window.eval`; the user restarts the app
//! explicitly via the "Restart Now" button (no automatic restart).

use log::{info, warn};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const WINDOW_LABEL: &str = "update-progress";
/// Custom URI scheme registered in `lib.rs` that serves the progress HTML.
pub const URI_SCHEME: &str = "aw-update";

const WINDOW_WIDTH: f64 = 420.0;
const WINDOW_HEIGHT: f64 = 240.0;

/// HTML for the progress dialog (also served by the custom protocol handler).
pub const PROGRESS_HTML: &str = include_str!("../assets/update-progress.html");

/// Opens (or focuses) the update progress window for the given version.
pub fn show_progress_window(app: &AppHandle, version: &str) -> Option<WebviewWindow> {
    if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
        let _ = existing.show();
        let _ = existing.set_focus();
        eval_set_version(&existing, version);
        return Some(existing);
    }

    // Custom scheme → valid Origin for IPC (data: URLs break invoke with
    // "Origin header is not a valid url").
    let url = match format!("{URI_SCHEME}://localhost/").parse() {
        Ok(url) => url,
        Err(e) => {
            warn!("Failed to parse update progress URL: {}", e);
            return None;
        }
    };

    let builder = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::CustomProtocol(url))
        .title("ActivityWatch Update")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .center()
        .always_on_top(true)
        .visible(true)
        // Queue UI updates that arrive before the page script defines the real handlers.
        .initialization_script(
            r#"
            (function () {
              var q = [];
              function enqueue(name, args) {
                q.push([name, args]);
              }
              window.__awUpdate = {
                setVersion: function () { enqueue('setVersion', arguments); },
                setProgress: function () { enqueue('setProgress', arguments); },
                setStatus: function () { enqueue('setStatus', arguments); },
                setInstalling: function () { enqueue('setInstalling', arguments); },
                setReady: function () { enqueue('setReady', arguments); },
                setError: function () { enqueue('setError', arguments); },
                __flush: function (impl) {
                  window.__awUpdate = impl;
                  for (var i = 0; i < q.length; i++) {
                    var name = q[i][0];
                    var args = q[i][1];
                    if (typeof impl[name] === 'function') {
                      impl[name].apply(impl, args);
                    }
                  }
                  q = [];
                }
              };
            })();
            "#,
        );

    match builder.build() {
        Ok(window) => {
            eval_set_version(&window, version);
            eval_set_status(&window, "Starting download…", None);
            info!("Opened update progress window for v{}", version);
            Some(window)
        }
        Err(e) => {
            warn!("Failed to create update progress window: {}", e);
            None
        }
    }
}

pub fn set_progress(window: &WebviewWindow, percent: f64, downloaded: usize, total: Option<u64>) {
    let total_js = match total {
        Some(t) => t.to_string(),
        None => "null".to_string(),
    };
    let script = format!(
        "window.__awUpdate && window.__awUpdate.setProgress({}, {}, {});",
        percent.clamp(0.0, 100.0),
        downloaded,
        total_js
    );
    if let Err(e) = window.eval(&script) {
        warn!("Failed to update progress UI: {}", e);
    }
}

pub fn set_status(window: &WebviewWindow, message: &str, kind: Option<&str>) {
    eval_set_status(window, message, kind);
}

pub fn set_installing(window: &WebviewWindow) {
    if let Err(e) = window.eval("window.__awUpdate && window.__awUpdate.setInstalling();") {
        warn!("Failed to set installing state in progress UI: {}", e);
    }
}

pub fn set_ready(window: &WebviewWindow) {
    if let Err(e) = window.eval("window.__awUpdate && window.__awUpdate.setReady();") {
        warn!("Failed to set ready state in progress UI: {}", e);
    }
    let _ = window.show();
    let _ = window.set_focus();
}

pub fn set_error(window: &WebviewWindow, message: &str) {
    let script = format!(
        "window.__awUpdate && window.__awUpdate.setError({});",
        serde_json::to_string(message).unwrap_or_else(|_| "\"Update failed.\"".into())
    );
    if let Err(e) = window.eval(&script) {
        warn!("Failed to set error state in progress UI: {}", e);
    }
    let _ = window.show();
    let _ = window.set_focus();
}

/// Close the progress window if it exists.
pub fn close_progress_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        if let Err(e) = window.close() {
            warn!("Failed to close update progress window: {}", e);
        }
    }
}

fn eval_set_version(window: &WebviewWindow, version: &str) {
    let script = format!(
        "window.__awUpdate && window.__awUpdate.setVersion({});",
        serde_json::to_string(version).unwrap_or_else(|_| "\"\"".into())
    );
    if let Err(e) = window.eval(&script) {
        warn!("Failed to set version in progress UI: {}", e);
    }
}

fn eval_set_status(window: &WebviewWindow, message: &str, kind: Option<&str>) {
    let kind_js = match kind {
        Some(k) => serde_json::to_string(k).unwrap_or_else(|_| "null".into()),
        None => "null".to_string(),
    };
    let script = format!(
        "window.__awUpdate && window.__awUpdate.setStatus({}, {});",
        serde_json::to_string(message).unwrap_or_else(|_| "\"\"".into()),
        kind_js
    );
    if let Err(e) = window.eval(&script) {
        warn!("Failed to set status in progress UI: {}", e);
    }
}
