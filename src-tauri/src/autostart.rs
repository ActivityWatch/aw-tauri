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

/// Serializes read-modify-write cycles on the config file (here and in
/// `crate::config_edit::persist_module_autostart`) so two concurrent
/// toggles cannot interleave and lose one of the writes.
pub(crate) static PERSIST_LOCK: Mutex<()> = Mutex::new(());

/// Last autostart state read from or written to the OS. Querying the OS can be slow (the macOS
/// AppleScript launcher runs `osascript` against System Events), and the tray menu is rebuilt
/// several times during startup, so menu rebuilds read this instead.
static REGISTERED: Mutex<Option<bool>> = Mutex::new(None);

fn set_cached(registered: bool) {
    *REGISTERED.lock().unwrap_or_else(|e| e.into_inner()) = Some(registered);
}

/// Forget the cached state so the next read goes back to the OS.
fn clear_cached() {
    *REGISTERED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether the app is currently registered to start at login, according to the OS.
pub fn is_registered(app: &AppHandle) -> Result<bool, String> {
    let registered = app
        .autolaunch()
        .is_enabled()
        .map_err(|e| format!("Failed to read autostart state: {e}"))?;
    set_cached(registered);
    Ok(registered)
}

/// Like [`is_registered`], but answers from the cache when the OS has already been queried.
fn is_registered_cached(app: &AppHandle) -> Result<bool, String> {
    let cached = *REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
    match cached {
        Some(registered) => Ok(registered),
        None => is_registered(app),
    }
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

    // A failed call may still have changed something; re-read the OS next time.
    apply(enabled).inspect_err(|_| clear_cached())?;

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
        match apply(previous) {
            Ok(()) => set_cached(previous),
            Err(rollback_err) => {
                // The OS state is now unknown; make the next read go back to the OS.
                clear_cached();
                error!("Failed to roll back autostart after a failed config write: {rollback_err}");
            }
        }
        return Err(e);
    }

    info!("Autostart set to {} (config and OS in sync)", enabled);
    Ok(actual)
}

/// Reconciles the autostart state at application startup.
///
/// The OS is treated as authoritative once a config file exists.  If the OS
/// entry disagrees with the stored config value, the config is updated to match
/// what the OS says — not the other way around.  This means that disabling
/// autostart outside the app (Task Manager's `StartupApproved`, macOS Login
/// Items, a desktop startup manager) is respected on the next launch rather
/// than silently re-enabled.
///
/// On a fresh install, before any config file has been written, the default
/// config value (`enabled = true`) is applied to the OS to set up the initial
/// login entry.  After that first run the OS state takes precedence.
pub fn sync_from_config(app: &AppHandle) {
    // migrate_legacy_* return Some(was_enabled) when they removed an owned legacy
    // entry: Some(true) = entry was active, Some(false) = entry was disabled in place.
    // Create the replacement only when the legacy entry was actually active so we do
    // not silently re-enable autostart that the user had turned off.
    let mut register = |enabled| {
        let manager = app.autolaunch();
        if enabled {
            manager.enable().map_err(|e| e.to_string())?;
        } else if manager.is_enabled().map_err(|e| e.to_string())? {
            manager.disable().map_err(|e| e.to_string())?;
        }
        if manager.is_enabled().map_err(|e| e.to_string())? != enabled {
            return Err(format!(
                "Replacement autostart entry did not reach enabled={enabled}"
            ));
        }
        set_cached(enabled);
        Ok(())
    };
    let migration =
        match migrate_legacy_named_profile_entry(&profile::current_profile(), &mut register) {
            Ok(migration) => migration,
            Err(e) => {
                clear_cached();
                warn!(
                    "Failed to migrate legacy autostart entry; keeping the original for retry: {e}"
                );
                return;
            }
        };

    // Named profiles must not migrate the default profile's LaunchAgent.
    #[cfg(target_os = "macos")]
    let migration = if migration.is_none() && profile::is_default(&profile::current_profile()) {
        match migrate_legacy_default_macos_launch_agent(&mut register) {
            Ok(migration) => migration,
            Err(e) => {
                clear_cached();
                warn!("Failed to migrate legacy LaunchAgent; keeping the original for retry: {e}");
                return;
            }
        }
    } else {
        migration
    };

    let config_enabled = get_config().autostart.enabled;
    let current = match is_registered(app) {
        Ok(current) => current,
        Err(e) => {
            warn!("Skipping autostart sync: {e}");
            return;
        }
    };

    if current == config_enabled {
        info!("Registered for autostart: {current} (in sync)");
        return;
    }

    // OS and config disagree.  How to reconcile depends on whether this is the
    // first run (no config file on disk yet) or a subsequent launch:
    //
    // • First run — config file does not exist: apply the default config to the
    //   OS to complete initial setup.
    // • Subsequent launch — config file exists: the user may have changed
    //   autostart outside the app.  Treat OS as the source of truth and persist
    //   its state into config so the next launch sees them in sync.
    //
    // Use crate::is_first_run() rather than get_config_path().exists(): by the
    // time we reach this point get_config() has already been called, which
    // writes the config file on a fresh install.  Checking the path here would
    // always return false on a new machine (file was just created), preventing
    // the initial autostart registration from ever running.
    let is_first_run = *crate::is_first_run();

    if apply_first_run_defaults(is_first_run, migration) {
        let manager = app.autolaunch();
        let result = if config_enabled {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(e) = result {
            warn!("Failed to set autostart to {config_enabled} on first run: {e}");
            clear_cached();
            return;
        }
        // Don't trust the call's Ok(()): it can silently do nothing.
        match is_registered(app) {
            Ok(actual) if actual == config_enabled => {
                info!("Registered for autostart: {config_enabled}")
            }
            Ok(actual) => warn!("Autostart is still {actual} after requesting {config_enabled}"),
            Err(e) => {
                warn!("{e}");
                clear_cached();
            }
        }
    } else {
        // OS wins: persist its state so config stays honest.
        info!(
            "OS autostart is {current} but config says {config_enabled}; \
             updating config to match OS (may have been changed outside the app)",
        );
        if let Err(e) = persist_enabled(current) {
            warn!("Failed to persist OS autostart state to config: {e}");
        }
    }
}

/// Migrate the default profile's pre-b7d5832 LaunchAgent to Login Items.
/// Keep the LaunchAgent until its replacement is registered and verified.
#[cfg(target_os = "macos")]
fn migrate_legacy_default_macos_launch_agent(
    register: &mut impl FnMut(bool) -> Result<(), String>,
) -> Result<Option<bool>, String> {
    let Some(path) = legacy_macos_launch_agent_path() else {
        return Ok(None);
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
    };
    if contents.contains("--profile") {
        return Ok(None);
    }
    finish_legacy_migration(true, register, || {
        fs::remove_file(&path).map_err(|e| format!("Failed to remove {}: {e}", path.display()))
    })
    .map(Some)
}

/// Migrate an owned shared-name login entry to this profile's identity.
/// `Some(false)` preserves a disabled legacy entry's intent; `None` means
/// no owned legacy entry. Failures retain the original and must stop sync.
fn migrate_legacy_named_profile_entry(
    profile: &str,
    register: &mut impl FnMut(bool) -> Result<(), String>,
) -> Result<Option<bool>, String> {
    if profile::is_default(profile) {
        return Ok(None);
    }

    #[cfg(target_os = "linux")]
    if let Some(path) = legacy_linux_desktop_path() {
        if let Some(was_enabled) = migrate_legacy_file_if_owned(&path, profile, register)? {
            return Ok(Some(was_enabled));
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(path) = legacy_macos_launch_agent_path() {
        if migrate_legacy_file_if_owned(&path, profile, register)?.is_some() {
            // A macOS LaunchAgent plist has no in-place "disabled" state;
            // its presence means the entry was active.
            return Ok(Some(true));
        }
    }

    #[cfg(windows)]
    {
        return migrate_legacy_windows_run_value(
            profile,
            profile::LEGACY_AUTOSTART_APP_NAME,
            register,
        );
    }

    #[cfg(not(windows))]
    Ok(None)
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

fn apply_first_run_defaults(first_run: bool, migration: Option<bool>) -> bool {
    first_run && migration.is_none()
}

/// Match and verify the replacement state before removing the old entry.
/// A failed registration leaves the original available on the next launch.
fn finish_legacy_migration(
    was_enabled: bool,
    register: &mut impl FnMut(bool) -> Result<(), String>,
    remove: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    register(was_enabled)?;
    remove()?;
    Ok(was_enabled)
}

fn migrate_legacy_file_if_owned(
    path: &Path,
    profile: &str,
    register: &mut impl FnMut(bool) -> Result<(), String>,
) -> Result<Option<bool>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Failed to read {}: {e}", path.display())),
    };
    if !profile::command_targets_profile(&contents, profile) {
        return Ok(None);
    }
    let was_enabled = !is_entry_hidden(&contents);
    finish_legacy_migration(was_enabled, register, || {
        fs::remove_file(path).map_err(|e| format!("Failed to remove {}: {e}", path.display()))
    })
    .map(Some)
}

/// Returns `true` when the file contents indicate the entry has been disabled
/// in place via an XDG desktop `Hidden=true` line.  macOS plists and Windows
/// registry values do not use this convention, so this always returns `false`
/// for those formats.
fn is_entry_hidden(contents: &str) -> bool {
    contents
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("hidden=true"))
}

#[cfg(windows)]
fn migrate_legacy_windows_run_value(
    profile: &str,
    name: &str,
    register: &mut impl FnMut(bool) -> Result<(), String>,
) -> Result<Option<bool>, String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_SET_VALUE) {
        Ok(key) => key,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Failed to open legacy Windows Run key: {e}")),
    };
    let value = match key.get_value::<String, _>(name) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Failed to read legacy Windows Run value: {e}")),
    };
    if !profile::command_targets_profile(&value, profile) {
        return Ok(None);
    }
    // Use the same Run + StartupApproved semantics as the autostart plugin.
    // Checking Run presence alone re-enables entries disabled in Task Manager.
    let was_enabled = auto_launch::AutoLaunch::new(name, "", &[] as &[&str])
        .is_enabled()
        .map_err(|e| format!("Failed to read legacy Windows autostart state: {e}"))?;
    finish_legacy_migration(was_enabled, register, || {
        key.delete_value(name)
            .map_err(|e| format!("Failed to remove legacy Windows Run value: {e}"))
    })
    .map(Some)
}

/// Builds the tray "Start at login" item, checked to match the current OS state.
pub fn build_menu_item(app: &AppHandle) -> CheckMenuItem<Wry> {
    let checked = is_registered_cached(app).unwrap_or_else(|e| {
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
        return crate::config_edit::write_atomic(&path, updated)
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
    use super::{
        apply_first_run_defaults, is_entry_hidden, migrate_legacy_file_if_owned,
        patch_autostart_enabled,
    };
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
    fn migration_failure_keeps_legacy_entry_for_retry() {
        let source = "Exec=/usr/bin/aw-tauri --profile research\nName=aw-tauri\n";
        let path = write_temp_entry("retry", source);
        let result = migrate_legacy_file_if_owned(&path, "research", &mut |_| {
            Err("registration failed".into())
        });
        let retained = fs::read_to_string(&path);
        assert!(result.is_err());
        assert_eq!(
            retained.unwrap(),
            source,
            "failed registration must retain the original"
        );
        let result = migrate_legacy_file_if_owned(&path, "research", &mut |_| {
            assert!(
                path.exists(),
                "original must remain until replacement is ready"
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(result, Some(true));
        assert!(!path.exists(), "successful retry removes the legacy entry");
    }

    #[test]
    fn migration_disabled_legacy_suppresses_first_run_defaults() {
        let path = write_temp_entry(
            "disabled-first-run",
            "Exec=aw-tauri --profile research\nHidden=true\n",
        );
        let migration = migrate_legacy_file_if_owned(&path, "research", &mut |enabled| {
            assert!(!enabled, "must not enable a disabled entry");
            Ok(())
        })
        .unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(migration, Some(false));
        assert!(
            !apply_first_run_defaults(true, migration),
            "disabled legacy intent must beat the first-run enabled default"
        );
    }

    #[test]
    fn migrate_removes_legacy_entry_owned_by_this_profile() {
        let path = write_temp_entry(
            "owned",
            "Exec=/usr/bin/aw-tauri --profile research\nName=aw-tauri\n",
        );
        assert_eq!(
            migrate_legacy_file_if_owned(&path, "research", &mut |_| Ok(())).unwrap(),
            Some(true),
            "owned enabled entry should be removed and report was_enabled=true"
        );
        assert!(!path.exists(), "owned leftover should be deleted");
    }

    #[test]
    fn migrate_keeps_legacy_entry_owned_by_default_or_other_profile() {
        let default_path = write_temp_entry("default", "Exec=/usr/bin/aw-tauri\nName=aw-tauri\n");
        let other_path = write_temp_entry(
            "other",
            "Exec=/usr/bin/aw-tauri --profile testing\nName=aw-tauri\n",
        );
        assert_eq!(
            migrate_legacy_file_if_owned(&default_path, "research", &mut |_| Ok(())).unwrap(),
            None
        );
        assert_eq!(
            migrate_legacy_file_if_owned(&other_path, "research", &mut |_| Ok(())).unwrap(),
            None
        );
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

    #[test]
    fn migrate_preserves_disabled_state_from_hidden_desktop_entry() {
        let path = write_temp_entry(
            "hidden",
            "Exec=/usr/bin/aw-tauri --profile research\nName=aw-tauri\nHidden=true\n",
        );
        assert_eq!(
            migrate_legacy_file_if_owned(&path, "research", &mut |_| Ok(())).unwrap(),
            Some(false),
            "owned disabled entry should be removed and report was_enabled=false"
        );
        assert!(!path.exists(), "disabled leftover should still be deleted");
    }

    #[test]
    fn is_entry_hidden_detects_xdg_hidden_field() {
        assert!(is_entry_hidden("Hidden=true\n"));
        assert!(is_entry_hidden("Exec=aw-tauri\nHidden=true\n"));
        assert!(is_entry_hidden("Exec=aw-tauri\nhidden=true\n")); // case-insensitive
        assert!(!is_entry_hidden("Exec=aw-tauri\nName=aw-tauri\n"));
        assert!(!is_entry_hidden("Hidden=false\n"));
        assert!(!is_entry_hidden("")); // macOS plist, Windows value: no Hidden field
    }
}

#[cfg(all(test, windows))]
#[path = "autostart_windows_tests.rs"]
mod windows_tests;
