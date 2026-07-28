//! Replaceable module-crash warning window.
//!
//! One shared webview lists failing modules. Re-crashes for the same module
//! update that row in place instead of stacking native OS dialogs. Successful
//! restarts update the same row when the window is already open.

use log::{info, warn};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const WINDOW_LABEL: &str = "module-alert";
/// Custom URI scheme registered in `lib.rs` that serves the alert HTML.
pub const URI_SCHEME: &str = "aw-module-alert";

const WINDOW_WIDTH: f64 = 440.0;
const WINDOW_HEIGHT: f64 = 320.0;

/// Serializes window create/update so concurrent module crashes do not race on
/// the fixed `module-alert` label and drop a status update.
static ALERT_LOCK: Mutex<()> = Mutex::new(());

/// HTML for the module alert dialog (also served by the custom protocol handler).
pub const ALERT_HTML: &str = include_str!("../assets/module-alert.html");

/// Visual kind for a module status row.
#[derive(Clone, Copy)]
pub enum StatusKind {
    Warning,
    Ok,
}

impl StatusKind {
    fn as_str(self) -> &'static str {
        match self {
            StatusKind::Warning => "warning",
            StatusKind::Ok => "ok",
        }
    }
}

/// Show or focus the module-alert window and set/update status for `module_name`.
pub fn show_or_update(app: &AppHandle, module_name: &str, message: &str, kind: StatusKind) {
    let _guard = ALERT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
        let _ = existing.show();
        let _ = existing.set_focus();
        eval_set_status(&existing, module_name, message, kind);
        return;
    }

    let url = match format!("{URI_SCHEME}://localhost/").parse() {
        Ok(url) => url,
        Err(e) => {
            warn!("Failed to parse module-alert URL: {}", e);
            return;
        }
    };

    let builder = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::CustomProtocol(url))
        .title("ActivityWatch")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(true)
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
              window.__awModuleAlert = {
                setStatus: function () { enqueue('setStatus', arguments); },
                __flush: function (impl) {
                  window.__awModuleAlert = impl;
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
            eval_set_status(&window, module_name, message, kind);
            info!("Opened module-alert window for {module_name}");
        }
        Err(e) => {
            // Another caller may have created the window between our check and build
            // (or the label is already registered). Apply status to the existing window.
            if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
                let _ = existing.show();
                let _ = existing.set_focus();
                eval_set_status(&existing, module_name, message, kind);
            } else {
                warn!("Failed to create module-alert window: {}", e);
            }
        }
    }
}

/// Update a module row only if the alert window is already open (e.g. recovery after restart).
/// Does not create or focus the window.
pub fn update_if_open(app: &AppHandle, module_name: &str, message: &str, kind: StatusKind) {
    let _guard = ALERT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
        eval_set_status(&existing, module_name, message, kind);
    }
}

/// Close the module-alert window if it exists.
#[allow(dead_code)]
pub fn close(app: &AppHandle) {
    let _guard = ALERT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        if let Err(e) = window.close() {
            warn!("Failed to close module-alert window: {}", e);
        }
    }
}

fn eval_set_status(window: &WebviewWindow, module_name: &str, message: &str, kind: StatusKind) {
    let name_js = serde_json::to_string(module_name).unwrap_or_else(|_| "\"unknown\"".to_string());
    let message_js =
        serde_json::to_string(message).unwrap_or_else(|_| "\"Something went wrong.\"".to_string());
    let kind_js = serde_json::to_string(kind.as_str()).unwrap_or_else(|_| "\"warning\"".into());
    let script = format!(
        "window.__awModuleAlert && window.__awModuleAlert.setStatus({}, {}, {});",
        name_js, message_js, kind_js
    );
    if let Err(e) = window.eval(&script) {
        warn!("Failed to update module-alert UI: {}", e);
    }
}
