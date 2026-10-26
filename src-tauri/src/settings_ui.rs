//! Settings window for editing the user config without touching TOML by hand.
//!
//! The page is served over a custom URI scheme (`aw-settings://`) so it has a
//! valid Origin for Tauri IPC, and it only talks to the app through the
//! commands below — nothing is exposed over HTTP. Most settings are read once
//! at startup, so saved changes take effect after a restart; start-at-login is
//! the exception and is applied to the OS immediately.

use log::{info, warn};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;

use crate::{autostart, get_config, get_config_path, write_formatted_config, UserConfig};

pub const WINDOW_LABEL: &str = "settings";
/// Custom URI scheme registered in `lib.rs` that serves the settings HTML.
pub const URI_SCHEME: &str = "aw-settings";
/// Tray menu id of the "Settings…" item.
pub const MENU_ID: &str = "open_settings";

const WINDOW_WIDTH: f64 = 560.0;
const WINDOW_HEIGHT: f64 = 680.0;

/// HTML for the settings window (served by the custom protocol handler).
pub const SETTINGS_HTML: &str = include_str!("../assets/settings.html");

/// Opens the settings window, or focuses it if it is already open.
pub fn show(app: &AppHandle) {
    if let Some(existing) = app.get_webview_window(WINDOW_LABEL) {
        let _ = existing.unminimize();
        let _ = existing.show();
        let _ = existing.set_focus();
        return;
    }

    let url = match format!("{URI_SCHEME}://localhost/").parse() {
        Ok(url) => url,
        Err(e) => {
            warn!("Failed to parse settings URL: {}", e);
            return;
        }
    };

    let result = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::CustomProtocol(url))
        .title("ActivityWatch Settings")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .min_inner_size(420.0, 400.0)
        .resizable(true)
        .center()
        .visible(true)
        .build();

    match result {
        Ok(window) => {
            let _ = window.set_focus();
            info!("Opened settings window");
        }
        Err(e) => warn!("Failed to create settings window: {}", e),
    }
}

/// Settings as shown in the window: the config file plus the live OS
/// autostart state, which is what the "Start at login" toggle reflects.
#[derive(serde::Serialize)]
pub struct SettingsPayload {
    config: UserConfig,
    config_path: String,
    available_modules: Vec<String>,
}

/// Reads the config from disk (not the copy cached at startup) so the window
/// shows edits made since launch, including ones made by hand.
pub fn load(app: &AppHandle) -> Result<SettingsPayload, String> {
    let path = get_config_path();
    let mut config = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<UserConfig>(&s).map_err(|e| {
            format!(
                "Config file {} is malformed; fix or remove it first: {e}",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => get_config().clone(),
        Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
    };
    if let Ok(registered) = autostart::is_registered(app) {
        config.autostart.enabled = registered;
    }
    Ok(SettingsPayload {
        config,
        config_path: path.display().to_string(),
        available_modules: crate::manager::discover_module_names(),
    })
}

/// Validates and writes `config`, then applies start-at-login to the OS.
pub fn save(app: &AppHandle, mut config: UserConfig) -> Result<(), String> {
    normalize(&mut config)?;

    let desired_autostart = config.autostart.enabled;
    {
        let _guard = autostart::persist_lock();
        // Persist the autostart value the OS currently has; `set_enabled`
        // below flips both together so they can never disagree.
        config.autostart.enabled =
            autostart::is_registered(app).unwrap_or(get_config().autostart.enabled);
        let path = get_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir {}: {e}", parent.display()))?;
        }
        write_formatted_config(&config, &path)
            .map_err(|e| format!("Failed to write config file {}: {e}", path.display()))?;
        info!("Settings saved to {}", path.display());
    }

    if config.autostart.enabled != desired_autostart {
        let actual = autostart::set_enabled(app, desired_autostart)?;
        autostart::sync_menu_item(actual);
    }
    Ok(())
}

/// Trims user input and rejects values the app could not start with.
fn normalize(config: &mut UserConfig) -> Result<(), String> {
    if config.port == 0 {
        return Err("Port must be between 1 and 65535".into());
    }

    config
        .discovery_paths
        .retain(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
    let mut seen = std::collections::HashSet::new();
    config.discovery_paths.retain(|p| seen.insert(p.clone()));

    for module in &mut config.autostart.modules {
        let (name, args) = match module {
            crate::ModuleEntry::Simple(name) => (name.trim().to_string(), String::new()),
            crate::ModuleEntry::Full { name, args } => {
                (name.trim().to_string(), args.trim().to_string())
            }
        };
        if name.is_empty() {
            return Err("Module names cannot be empty".into());
        }
        *module = if args.is_empty() {
            crate::ModuleEntry::Simple(name)
        } else {
            crate::ModuleEntry::Full { name, args }
        };
    }
    Ok(())
}

/// Shows a native folder picker attached to the settings window.
pub fn pick_directory(app: &AppHandle) -> Option<String> {
    let mut dialog = app.dialog().file().set_title("Add module search folder");
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        dialog = dialog.set_parent(&window);
    }
    dialog.blocking_pick_folder().map(|p| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize;
    use crate::{ModuleEntry, UserConfig};
    use std::path::PathBuf;

    #[test]
    fn normalize_rejects_port_zero() {
        let mut config = UserConfig {
            port: 0,
            ..UserConfig::default()
        };
        assert!(normalize(&mut config).is_err());
    }

    #[test]
    fn normalize_trims_modules_and_drops_empty_args() {
        let mut config = UserConfig::default();
        config.autostart.modules = vec![
            ModuleEntry::Full {
                name: " aw-watcher-afk ".into(),
                args: "  ".into(),
            },
            ModuleEntry::Full {
                name: "aw-sync".into(),
                args: " daemon ".into(),
            },
        ];
        normalize(&mut config).unwrap();
        assert!(
            matches!(&config.autostart.modules[0], ModuleEntry::Simple(n) if n == "aw-watcher-afk")
        );
        assert!(matches!(
            &config.autostart.modules[1],
            ModuleEntry::Full { name, args } if name == "aw-sync" && args == "daemon"
        ));
    }

    #[test]
    fn normalize_rejects_blank_module_names() {
        let mut config = UserConfig::default();
        config.autostart.modules = vec![ModuleEntry::Simple("   ".into())];
        assert!(normalize(&mut config).is_err());
    }

    #[test]
    fn normalize_drops_blank_and_duplicate_paths() {
        let mut config = UserConfig {
            discovery_paths: vec![
                PathBuf::from("/opt/aw"),
                PathBuf::from(""),
                PathBuf::from("/opt/aw"),
            ],
            ..UserConfig::default()
        };
        normalize(&mut config).unwrap();
        assert_eq!(config.discovery_paths, vec![PathBuf::from("/opt/aw")]);
    }
}
