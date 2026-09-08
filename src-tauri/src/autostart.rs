//! User-facing control over "start at login".
//!
//! OS-level registration is performed by `tauri-plugin-autostart`; the desired
//! state is persisted as `[autostart] enabled` in the user config. Those two
//! must never disagree, so every mutation here goes through [`set_enabled`],
//! which applies the OS change, verifies it took effect, persists it, and rolls
//! the OS change back if persisting fails.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use log::{error, info, warn};
use tauri::menu::CheckMenuItem;
use tauri::{AppHandle, Wry};
use tauri_plugin_autostart::ManagerExt;

use crate::profile;
use crate::{get_config, get_config_path, write_formatted_config, UserConfig};

/// Menu id of the tray "Start at login" item.
pub const MENU_ID: &str = "autostart";

/// The tray check item, kept so the checkmark can be restored when a toggle
/// fails and re-synced when the state changes from elsewhere (e.g. the
/// `set_autostart_enabled` command). Rebuilt whenever the tray menu is rebuilt.
static MENU_ITEM: Mutex<Option<CheckMenuItem<Wry>>> = Mutex::new(None);

/// Serializes read-modify-write cycles on the config file so two concurrent
/// toggles cannot interleave and lose one of the writes.
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

/// Whether the app is currently registered to start at login, according to the OS.
pub fn is_registered(app: &AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("Failed to read autostart state: {e}"))
}

/// Registers or unregisters the app for autostart and persists the choice.
///
/// Returns the state as read back from the OS. Errors leave config and OS
/// agreeing on the *previous* value rather than a half-applied one.
pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    let previous = is_registered(app)?;

    let apply = |value: bool| -> Result<(), String> {
        let result = if value {
            manager.enable()
        } else {
            manager.disable()
        };
        result.map_err(|e| {
            format!(
                "Failed to {} autostart: {e}",
                if value { "enable" } else { "disable" }
            )
        })
    };

    apply(enabled)?;

    // Don't trust the call's Ok(()) — read the OS back so a silent no-op can't
    // be persisted as success.
    let actual = is_registered(app)?;
    if actual != enabled {
        return Err(format!(
            "Autostart is still {} after requesting {}",
            actual, enabled
        ));
    }

    if let Err(e) = persist_enabled(enabled) {
        // Config and OS would now disagree; undo the OS change so they don't.
        if let Err(rollback_err) = apply(previous) {
            error!("Failed to roll back autostart after a failed config write: {rollback_err}");
        }
        return Err(e);
    }

    info!("Autostart set to {} (config and OS in sync)", enabled);
    Ok(actual)
}

/// Applies `[autostart] enabled` from the config to the OS at startup.
///
/// The config is authoritative here — this is what makes a hand-edited config
/// file take effect — and it is applied in *both* directions, so clearing the
/// flag also removes an already-registered login item.
pub fn sync_from_config(app: &AppHandle) {
    migrate_legacy_named_profile_entry(&profile::current_profile());

    let desired = get_config().autostart.enabled;
    let current = match is_registered(app) {
        Ok(current) => current,
        Err(e) => {
            warn!("Skipping autostart sync: {e}");
            return;
        }
    };

    if current == desired {
        info!("Registered for autostart: {desired} (already in sync)");
        return;
    }

    let manager = app.autolaunch();
    let result = if desired {
        manager.enable()
    } else {
        manager.disable()
    };
    match result {
        Ok(()) => info!("Registered for autostart: {desired}"),
        // A missing/read-only autostart directory shouldn't stop the app from starting.
        Err(e) => warn!("Failed to set autostart to {desired}: {e}"),
    }
}

/// Drop a leftover shared `aw-tauri` login entry if a previous release
/// registered this named profile under that identity. The default profile
/// still owns that name, so we only remove the entry when its command
/// contains `--profile <this>`.
fn migrate_legacy_named_profile_entry(profile: &str) {
    if profile::is_default(profile) {
        return;
    }

    #[cfg(target_os = "linux")]
    if let Some(path) = legacy_linux_desktop_path() {
        remove_legacy_file_if_owned(&path, profile);
    }

    #[cfg(target_os = "macos")]
    if let Some(path) = legacy_macos_launch_agent_path() {
        remove_legacy_file_if_owned(&path, profile);
    }

    #[cfg(windows)]
    migrate_legacy_windows_run_value(profile);
}

#[cfg(target_os = "linux")]
fn legacy_linux_desktop_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".config/autostart")
            .join(format!("{}.desktop", profile::LEGACY_AUTOSTART_APP_NAME))
    })
}

#[cfg(target_os = "macos")]
fn legacy_macos_launch_agent_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| {
        home.join("Library/LaunchAgents")
            .join(format!("{}.plist", profile::LEGACY_AUTOSTART_APP_NAME))
    })
}

fn remove_legacy_file_if_owned(path: &Path, profile: &str) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    if !profile::command_targets_profile(&contents, profile) {
        return false;
    }
    match fs::remove_file(path) {
        Ok(()) => {
            info!(
                "Removed leftover autostart entry at {} owned by profile {profile}",
                path.display()
            );
            true
        }
        Err(e) => {
            warn!(
                "Failed to remove leftover autostart entry {}: {e}",
                path.display()
            );
            false
        }
    }
}

#[cfg(windows)]
fn migrate_legacy_windows_run_value(profile: &str) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) else {
        return;
    };
    let Ok(value) = key.get_value::<String, _>(profile::LEGACY_AUTOSTART_APP_NAME) else {
        return;
    };
    if !profile::command_targets_profile(&value, profile) {
        return;
    }
    match hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        Ok(key) => match key.delete_value(profile::LEGACY_AUTOSTART_APP_NAME) {
            Ok(()) => info!(
                "Removed leftover Windows Run value {} owned by profile {profile}",
                profile::LEGACY_AUTOSTART_APP_NAME
            ),
            Err(e) => warn!("Failed to remove leftover Windows Run value: {e}"),
        },
        Err(e) => warn!("Failed to open Windows Run key for leftover removal: {e}"),
    }
}

/// Builds the tray "Start at login" item, checked to match the current OS state.
pub fn build_menu_item(app: &AppHandle) -> CheckMenuItem<Wry> {
    let checked = is_registered(app).unwrap_or_else(|e| {
        warn!("{e}; falling back to the configured value for the tray checkmark");
        get_config().autostart.enabled
    });
    let item = CheckMenuItem::with_id(app, MENU_ID, "Start at login", true, checked, None::<&str>)
        .expect("Failed to create autostart menu item");
    *MENU_ITEM.lock().unwrap_or_else(|e| e.into_inner()) = Some(item.clone());
    item
}

/// Points the tray checkmark at `checked`, if the tray menu has been built.
pub fn sync_menu_item(checked: bool) {
    if let Some(item) = MENU_ITEM.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if let Err(e) = item.set_checked(checked) {
            warn!("Failed to update the autostart tray checkmark: {e}");
        }
    }
}

/// Undoes the checkmark flip that a click performs before the event is handled.
fn revert_menu_item() {
    let guard = MENU_ITEM.lock().unwrap_or_else(|e| e.into_inner());
    let Some(item) = guard.as_ref() else { return };
    match item.is_checked() {
        Ok(checked) => {
            if let Err(e) = item.set_checked(!checked) {
                warn!("Failed to restore the autostart tray checkmark: {e}");
            }
        }
        Err(e) => warn!("Failed to read the autostart tray checkmark: {e}"),
    }
}

/// Handles a click on the tray "Start at login" item.
///
/// The new value is derived from the OS state rather than from the check item,
/// since the item has already flipped itself by the time this runs.
pub fn handle_menu_click(app: &AppHandle) {
    let desired = match is_registered(app) {
        Ok(current) => !current,
        Err(e) => {
            error!("{e}");
            revert_menu_item();
            report_failure(app, &e);
            return;
        }
    };

    match set_enabled(app, desired) {
        Ok(actual) => sync_menu_item(actual),
        Err(e) => {
            error!("{e}");
            // The click already flipped the checkmark; put it back.
            sync_menu_item(!desired);
            report_failure(app, &e);
        }
    }
}

fn report_failure(app: &AppHandle, message: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

    app.dialog()
        .message(format!(
            "Could not change the start-at-login setting.\n\n{message}"
        ))
        .kind(MessageDialogKind::Error)
        .title("Autostart")
        .show(|_| {});
}

/// Writes `enabled` to the `[autostart]` section of the config file.
///
/// The file is re-read from disk rather than reusing the config loaded at
/// startup, so a toggle doesn't revert edits made in the meantime.
fn persist_enabled(enabled: bool) -> Result<(), String> {
    let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = get_config_path();

    if let Some(updated) = read_and_patch(&path, enabled) {
        return std::fs::write(&path, updated)
            .map_err(|e| format!("Failed to write config file {}: {e}", path.display()));
    }

    // Surgical patch unavailable (no `[autostart] enabled` key, or the
    // patched result didn't parse). Re-read the on-disk config for a full
    // rewrite. If the file exists but is malformed we return an error rather
    // than silently overwriting it with the startup-cached values — that would
    // discard edits the user made to unrelated fields after startup.
    let mut config = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // New install: no file yet, safe to seed from startup defaults.
            get_config().clone()
        }
        Err(e) => {
            return Err(format!(
                "Failed to read config file {}: {e}",
                path.display()
            ));
        }
        Ok(s) => toml::from_str::<UserConfig>(&s).map_err(|e| {
            format!(
                "Config file {} is malformed and cannot be updated safely: {e}",
                path.display()
            )
        })?,
    };
    config.autostart.enabled = enabled;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir {}: {e}", parent.display()))?;
    }
    write_formatted_config(&config, &path)
        .map_err(|e| format!("Failed to write config file {}: {e}", path.display()))
}

/// Reads the config file and returns it with `[autostart] enabled` set to
/// `enabled`, or `None` if the file can't be read or patched safely.
fn read_and_patch(path: &Path, enabled: bool) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    let patched = patch_autostart_enabled(&source, enabled)?;
    // Only accept the surgical edit if the result still parses and actually
    // holds the requested value; otherwise let the caller rewrite the file.
    match toml::from_str::<UserConfig>(&patched) {
        Ok(config) if config.autostart.enabled == enabled => Some(patched),
        Ok(_) => None,
        Err(e) => {
            warn!("Patched config did not parse ({e}); rewriting it instead");
            None
        }
    }
}

/// Rewrites the `enabled` key of the `[autostart]` table, preserving every
/// other byte of the document — comments and hand-formatting included.
///
/// Handles both section-style (`[autostart]\nenabled = …`) and dotted-key style
/// (`autostart.enabled = …`) so valid TOML written either way is patched in
/// place rather than falling through to the full-rewrite path.
///
/// Returns `None` when there is no such key to replace.
fn patch_autostart_enabled(source: &str, enabled: bool) -> Option<String> {
    let mut out = String::with_capacity(source.len() + 8);
    let mut in_autostart = false;
    let mut replaced = false;

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();

        if trimmed.starts_with('[') {
            let header = match trimmed.find(']') {
                Some(end) => trimmed[..=end].trim(),
                None => trimmed.trim_end(),
            };
            in_autostart = header == "[autostart]";
        } else if !replaced {
            // Section-style: inside [autostart], match `enabled = …`
            let matched_key = if in_autostart {
                trimmed
                    .split_once('=')
                    .filter(|(k, _)| k.trim_end() == "enabled")
                    .map(|_| "enabled")
            } else {
                // Dotted-key style: `autostart.enabled = …` at file scope.
                // `toml` accepts this as an equivalent form; the patcher must
                // recognise it so comments and other fields survive the toggle.
                is_dotted_autostart_enabled(trimmed).then_some("autostart.enabled")
            };

            if let Some(key) = matched_key {
                let indent = &line[..line.len() - trimmed.len()];
                let newline = if line.ends_with("\r\n") {
                    "\r\n"
                } else if line.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                out.push_str(indent);
                out.push_str(&format!("{key} = {enabled}{newline}"));
                replaced = true;
                continue;
            }
        }

        out.push_str(line);
    }

    replaced.then_some(out)
}

/// Returns `true` when `trimmed` is a dotted-key assignment for
/// `autostart.enabled`, i.e., matches `autostart[ws].[ws]enabled[ws]=…`.
fn is_dotted_autostart_enabled(trimmed: &str) -> bool {
    (|| -> Option<()> {
        let rest = trimmed.strip_prefix("autostart")?;
        let rest = rest.trim_start().strip_prefix('.')?;
        let rest = rest.trim_start().strip_prefix("enabled")?;
        rest.trim_start().strip_prefix('=')?;
        Some(())
    })()
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::{patch_autostart_enabled, remove_legacy_file_if_owned};
    use std::fs;

    const CONFIG: &str = r#"port = 5600
discovery_paths = []

[autostart]
# should I start with the system?
enabled = true
minimized = true
modules = [
  "aw-watcher-afk",
]

[updates]
auto_download = true
"#;

    #[test]
    fn patch_flips_the_value_and_keeps_everything_else() {
        let patched = patch_autostart_enabled(CONFIG, false).expect("should patch");
        assert!(patched.contains("enabled = false"));
        assert!(patched.contains("# should I start with the system?"));
        assert!(patched.contains("minimized = true"));
        assert!(patched.contains("auto_download = true"));
        assert_eq!(patched.lines().count(), CONFIG.lines().count());
    }

    #[test]
    fn patch_only_touches_the_autostart_table() {
        let source = "[updates]\nenabled = true\n\n[autostart]\nenabled = true\n";
        let patched = patch_autostart_enabled(source, false).expect("should patch");
        assert_eq!(
            patched,
            "[updates]\nenabled = true\n\n[autostart]\nenabled = false\n"
        );
    }

    #[test]
    fn patch_handles_indentation_and_a_commented_header() {
        let source = "[autostart] # tray toggle writes here\n  enabled   =   false\n";
        let patched = patch_autostart_enabled(source, true).expect("should patch");
        assert_eq!(
            patched,
            "[autostart] # tray toggle writes here\n  enabled = true\n"
        );
    }

    #[test]
    fn patch_reports_a_missing_key() {
        assert!(patch_autostart_enabled("[autostart]\nminimized = true\n", true).is_none());
        assert!(patch_autostart_enabled("port = 5600\n", true).is_none());
    }

    #[test]
    fn patch_handles_dotted_key_syntax() {
        // TOML allows `autostart.enabled = true` as an alternative to the
        // section-header form; the patcher must handle it in place so comments
        // and other fields are not lost through the full-rewrite fallback.
        let source = "port = 5600\n# comment\nautostart.enabled = false\n";
        let patched = patch_autostart_enabled(source, true).expect("should patch dotted-key form");
        assert!(patched.contains("autostart.enabled = true"), "{patched}");
        assert!(
            patched.contains("port = 5600"),
            "other fields preserved: {patched}"
        );
        assert!(
            patched.contains("# comment"),
            "comment preserved: {patched}"
        );
        assert_eq!(patched.lines().count(), source.lines().count());
    }

    #[test]
    fn patch_handles_dotted_key_with_spaces_around_dot() {
        let source = "autostart . enabled = true\n";
        let patched =
            patch_autostart_enabled(source, false).expect("should patch spaced dotted-key");
        assert!(patched.contains("autostart.enabled = false"), "{patched}");
    }

    #[test]
    fn patch_preserves_crlf_line_endings() {
        let patched =
            patch_autostart_enabled("[autostart]\r\nenabled = true\r\n", false).expect("patch");
        assert_eq!(patched, "[autostart]\r\nenabled = false\r\n");
    }

    fn write_temp_entry(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aw-tauri-legacy-autostart-{}-{}-{name}.desktop",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, contents).expect("write temp autostart entry");
        path
    }

    #[test]
    fn migrate_removes_legacy_entry_owned_by_this_profile() {
        let path = write_temp_entry(
            "owned",
            "Exec=/usr/bin/aw-tauri --profile research\nName=aw-tauri\n",
        );
        assert!(remove_legacy_file_if_owned(&path, "research"));
        assert!(!path.exists(), "owned leftover should be deleted");
    }

    #[test]
    fn migrate_keeps_legacy_entry_owned_by_default_or_other_profile() {
        let default_path = write_temp_entry("default", "Exec=/usr/bin/aw-tauri\nName=aw-tauri\n");
        let other_path = write_temp_entry(
            "other",
            "Exec=/usr/bin/aw-tauri --profile testing\nName=aw-tauri\n",
        );
        assert!(!remove_legacy_file_if_owned(&default_path, "research"));
        assert!(!remove_legacy_file_if_owned(&other_path, "research"));
        assert!(
            default_path.exists(),
            "default identity must be left intact"
        );
        assert!(
            other_path.exists(),
            "other named leftover must be left intact"
        );
        let _ = fs::remove_file(&default_path);
        let _ = fs::remove_file(&other_path);
    }
}
