//! Directory management for ActivityWatch Tauri
//!
//! Supported platforms: Windows, Linux, macOS, Android
//!
//! Isolation is at the platformdirs appname root. `default` and `testing`
//! keep the bare `activitywatch` name so existing installs are not orphaned;
//! any other profile (`research`, …) gets a sibling root `activitywatch-<p>`.
//! Module segments (`aw-tauri`, …) do not change.

use std::fs;
use std::path::{Path, PathBuf};

use crate::profile::{current_profile, DEFAULT_PROFILE, TESTING_PROFILE};

/// Platform "appname" root for the current profile.
#[cfg(not(target_os = "android"))]
fn appname() -> String {
    appname_for(&current_profile())
}

/// `default` and `testing` keep the legacy bare root — existing installs must
/// not be orphaned. Any other profile gets its own sibling root.
#[cfg(not(target_os = "android"))]
fn appname_for(profile: &str) -> String {
    if profile == DEFAULT_PROFILE || profile == TESTING_PROFILE {
        "activitywatch".to_string()
    } else {
        format!("activitywatch-{profile}")
    }
}

#[cfg(target_os = "android")]
use std::sync::Mutex;

#[cfg(target_os = "android")]
use lazy_static::lazy_static;

#[cfg(target_os = "android")]
lazy_static! {
    static ref ANDROID_DATA_DIR: Mutex<PathBuf> =
        Mutex::new(PathBuf::from("/data/user/0/net.activitywatch.app/files"));
}

#[cfg(not(target_os = "android"))]
pub fn get_config_dir() -> Result<PathBuf, ()> {
    config_dir_in(&appname())
}

/// The dir aw-tauri's own config/data live in, under a platform root.
#[cfg(not(target_os = "android"))]
const MODULE: &str = "aw-tauri";

/// `<config root>/<appname>/aw-tauri`. `%LOCALAPPDATA%` on Windows (not
/// Roaming), first moving over anything v0.14.0 put under Roaming, with
/// aw-server-rust's resolver so aw-tauri and aw-server follow one rule: see
/// `aw_server::dirs::module_dir` / `choose_module_dir`.
#[cfg(not(target_os = "android"))]
fn config_dir_in(appname: &str) -> Result<PathBuf, ()> {
    Ok(aw_server::dirs::module_dir(
        aw_server::dirs::user_config_root().ok_or(())?,
        appname,
        MODULE,
    ))
}

#[cfg(target_os = "android")]
pub fn get_config_dir() -> Result<PathBuf, ()> {
    panic!("not implemented on Android");
}

#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
pub fn get_data_dir() -> Result<PathBuf, ()> {
    data_dir_in(&appname())
}

/// `<data root>/<appname>/aw-tauri`; Windows handling as in [`config_dir_in`].
#[cfg(not(target_os = "android"))]
fn data_dir_in(appname: &str) -> Result<PathBuf, ()> {
    Ok(aw_server::dirs::module_dir(
        aw_server::dirs::user_data_root().ok_or(())?,
        appname,
        MODULE,
    ))
}

#[cfg(target_os = "android")]
pub fn get_data_dir() -> Result<PathBuf, ()> {
    Ok(ANDROID_DATA_DIR
        .lock()
        .expect("Unable to create data dir")
        .to_path_buf())
}

#[cfg(not(target_os = "android"))]
pub fn get_log_dir() -> Result<PathBuf, ()> {
    log_dir_in(&appname())
}

#[cfg(target_os = "linux")]
fn log_dir_in(appname: &str) -> Result<PathBuf, ()> {
    // Linux uses cache dir for logs
    let dir = dirs::cache_dir()
        .ok_or(())?
        .join(appname)
        .join("aw-tauri")
        .join("log");
    fs::create_dir_all(&dir).expect("Unable to create log dir");
    Ok(dir)
}

#[cfg(target_os = "windows")]
fn log_dir_in(appname: &str) -> Result<PathBuf, ()> {
    // Windows: %LOCALAPPDATA%\<appname>\Logs\aw-tauri
    let dir = dirs::data_local_dir()
        .ok_or(())?
        .join(appname)
        .join("Logs")
        .join("aw-tauri");
    fs::create_dir_all(&dir).expect("Unable to create log dir");
    Ok(dir)
}

#[cfg(all(
    not(target_os = "android"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
fn log_dir_in(appname: &str) -> Result<PathBuf, ()> {
    // macOS: ~/Library/Logs/<appname>/aw-tauri
    let dir = dirs::home_dir()
        .ok_or(())?
        .join("Library")
        .join("Logs")
        .join(appname)
        .join("aw-tauri");
    fs::create_dir_all(&dir).expect("Unable to create log dir");
    Ok(dir)
}

#[cfg(target_os = "android")]
pub fn get_log_dir() -> Result<PathBuf, ()> {
    panic!("not implemented on Android");
}

pub fn get_config_path() -> PathBuf {
    get_config_dir()
        .expect("Failed to get config dir")
        .join("config.toml")
}

pub fn get_log_path() -> PathBuf {
    get_log_dir()
        .expect("Failed to get log dir")
        .join("aw-tauri.log")
}

#[cfg(target_os = "linux")]
pub fn get_runtime_dir() -> PathBuf {
    // Linux: use XDG_RUNTIME_DIR or fallback to cache dir
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(runtime_dir).join(appname()).join("aw-tauri");
        if fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    // Fallback to cache dir
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(appname())
        .join("aw-tauri");
    let _ = fs::create_dir_all(&dir);
    dir
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn get_runtime_dir() -> PathBuf {
    // For Windows and macOS, use data directory for runtime files
    get_data_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(target_os = "android")]
pub fn get_runtime_dir() -> PathBuf {
    get_data_dir().unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Paths next to the installed app binary where bundled modules live.
///
/// Resolved at runtime from `current_exe()` (and `APPDIR` on AppImage) so
/// upgrades still find modules even when `config.toml` was written by an older
/// build that did not list these paths.
///
/// Layout (Tauri `bundle.resources`):
/// - Linux deb/rpm: `/usr/lib/aw-tauri/modules/` (binary in `/usr/bin/`)
/// - Linux AppImage: `$APPDIR/usr/lib/aw-tauri/modules/`
/// - macOS: `Contents/Resources/modules/` (and legacy `Contents/Resources/`)
/// - Windows: the install dir holding `aw-tauri.exe` and the `aw-watcher-*\`
///   subdirs (the Inno installer also places a copy in `<app>\aw-tauri\`)
pub fn get_install_discovery_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        linux_install_discovery_paths(
            std::env::current_exe().ok().as_deref(),
            std::env::var_os("APPDIR").map(PathBuf::from).as_deref(),
        )
    }

    #[cfg(target_os = "macos")]
    {
        macos_install_discovery_paths(std::env::current_exe().ok().as_deref())
    }

    #[cfg(target_os = "windows")]
    {
        windows_install_discovery_paths(std::env::current_exe().ok().as_deref())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Windows layout, split out from `get_install_discovery_paths` so it can be
/// tested against a fake install tree.
///
/// The Inno installer (`aw-tauri.iss`) copies the bundle into `<app>\` (so
/// `<app>\aw-tauri.exe` sits next to `<app>\aw-watcher-*\`) and also puts
/// `aw-tauri.exe` in `<app>\aw-tauri\`. When running from that subdir, the
/// bundled modules are in its parent.
#[cfg(any(target_os = "windows", test))]
fn windows_install_discovery_paths(exe_path: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(exe_dir) = exe_path.and_then(Path::parent) {
        paths.push(exe_dir.to_path_buf());
        let in_aw_tauri_subdir = exe_dir
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("aw-tauri"));
        if in_aw_tauri_subdir {
            if let Some(app_dir) = exe_dir.parent() {
                paths.push(app_dir.to_path_buf());
            }
        }
    }

    paths
}

/// Linux layout, split out from `get_install_discovery_paths` so it can be
/// tested against a fake install tree.
#[cfg(any(target_os = "linux", test))]
fn linux_install_discovery_paths(exe_path: Option<&Path>, appdir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(exe_dir) = exe_path.and_then(Path::parent) {
        // externalBin / same-directory layout
        paths.push(exe_dir.to_path_buf());

        // Tauri resources: ../lib/<productName>/ relative to the binary
        if let Some(prefix) = exe_dir.parent() {
            let resource = prefix.join("lib").join("aw-tauri");
            if resource.exists() {
                paths.push(resource.join("modules"));
                paths.push(resource);
            }
        }
    }

    // AppImage runtime sets APPDIR to the mounted squashfs root
    if let Some(appdir) = appdir {
        let resource = appdir.join("usr").join("lib").join("aw-tauri");
        if resource.exists() {
            let modules = resource.join("modules");
            if !paths.contains(&modules) {
                paths.push(modules);
            }
            if !paths.contains(&resource) {
                paths.push(resource);
            }
        }
    }

    paths
}

/// macOS layout, split out from `get_install_discovery_paths` so it can be
/// tested against a fake app bundle.
#[cfg(any(target_os = "macos", test))]
fn macos_install_discovery_paths(exe_path: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Structure: Contents/MacOS/aw-tauri -> go up two levels -> Contents/Resources
    if let Some(contents_dir) = exe_path.and_then(Path::parent).and_then(Path::parent) {
        let resources_dir = contents_dir.join("Resources");
        if resources_dir.exists() {
            // Modules bundled via tauri.conf.json `bundle.resources` land in Resources/modules/.
            paths.push(resources_dir.join("modules"));
            // Also include Resources/ directly for compatibility with modules placed
            // at the root (e.g. legacy build_app_tauri.sh layout).
            paths.push(resources_dir);
        }
    }

    paths
}

/// User-level discovery paths, written into a new `config.toml` as defaults.
///
/// Install-relative paths (`get_install_discovery_paths`) are deliberately not
/// included: they depend on where this binary runs from (and for an AppImage,
/// a per-run mount point), so persisting them would pin a later install to an
/// old one's modules. The module manager adds them at runtime instead.
pub fn get_discovery_paths() -> Vec<PathBuf> {
    let mut discovery_paths = Vec::new();

    #[cfg(target_os = "linux")]
    {
        // Linux: XDG-compliant paths
        if let Ok(home_dir) = std::env::var("HOME") {
            let home_path = PathBuf::from(&home_dir);

            // User executables directories
            discovery_paths.push(home_path.join("bin")); // ~/bin (traditional)
            discovery_paths.push(home_path.join(".local").join("bin")); // ~/.local/bin (modern XDG)

            // XDG_DATA_HOME or ~/.local/share (user data)
            let data_dir = std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home_path.join(".local").join("share"));
            discovery_paths.push(
                data_dir
                    .join("activitywatch")
                    .join("aw-tauri")
                    .join("modules"),
            );

            // Legacy path for backward compatibility
            discovery_paths.push(home_path.join("aw-modules"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Windows: User-specific and system paths
        if let Ok(username) = std::env::var("USERNAME") {
            discovery_paths.push(PathBuf::from(format!(r"C:/Users/{}/aw-modules", username)));
            discovery_paths.push(PathBuf::from(format!(
                r"C:/Users/{}/AppData/Local/Programs/ActivityWatch-Tauri",
                username
            )));
            discovery_paths.push(PathBuf::from(format!(
                r"C:/Users/{}/AppData/Local/Programs/ActivityWatch",
                username
            )));
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macOS: Application bundle and user paths
        if let Ok(home_dir) = std::env::var("HOME") {
            discovery_paths.push(PathBuf::from(home_dir).join("aw-modules"));
        }
    }

    #[cfg(target_os = "android")]
    {
        // Android: No discovery paths needed for mobile platform
    }

    discovery_paths
}

#[cfg(target_os = "android")]
pub fn set_android_data_dir(path: &str) {
    let mut android_data_dir = ANDROID_DATA_DIR
        .lock()
        .expect("Unable to acquire ANDROID_DATA_DIR lock");
    *android_data_dir = PathBuf::from(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_dirs() {
        #[cfg(target_os = "android")]
        set_android_data_dir("/test");

        #[cfg(not(target_os = "android"))]
        {
            get_config_dir().expect("Failed to get config directory");
            get_log_dir().expect("Failed to get log directory");
        }

        get_data_dir().expect("Failed to get data directory");

        let _ = get_config_path();
        let _ = get_log_path();
        let _ = get_runtime_dir();
        let _ = get_discovery_paths();
    }

    #[test]
    fn test_paths_exist() {
        #[cfg(target_os = "android")]
        set_android_data_dir("/test");

        #[cfg(not(target_os = "android"))]
        {
            let config_path = get_config_path();
            let log_path = get_log_path();

            // The parent directories should exist after calling the functions
            assert!(config_path.parent().unwrap().exists());
            assert!(log_path.parent().unwrap().exists());
        }
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn test_appname_root_isolation() {
        // default and testing keep the legacy bare root — existing installs
        // must not be orphaned by this change.
        assert_eq!(appname_for("default"), "activitywatch");
        assert_eq!(appname_for("testing"), "activitywatch");

        // any other profile gets its own sibling root
        assert_eq!(appname_for("research"), "activitywatch-research");
        assert_eq!(appname_for("my-profile"), "activitywatch-my-profile");
    }

    /// Pins aw-tauri's on-disk locations per platform, derived from env vars
    /// rather than the `dirs` crate, so a dependency change that moves them
    /// (as #203 did on Windows: %LOCALAPPDATA% -> Roaming %APPDATA%) fails here.
    /// Matches <https://docs.activitywatch.net/en/latest/directories.html>;
    /// change together with the sibling pins in aw-server-rust
    /// (`aw-server/src/dirs.rs` `test_default_paths_are_pinned`) and aw-core
    /// (`tests/test_dirs_pinned.py`).
    /// Do not update these without a migration for existing installs.
    #[test]
    #[cfg(not(target_os = "android"))]
    fn test_default_paths_are_pinned() {
        let env = |v: &str| PathBuf::from(std::env::var(v).unwrap());
        #[cfg(target_os = "windows")]
        let (data, config, log) = {
            let app = env("LOCALAPPDATA").join("activitywatch");
            (app.clone(), app.clone(), app.join("Logs").join("aw-tauri"))
        };
        #[cfg(target_os = "macos")]
        let (data, config, log) = {
            let support = env("HOME").join("Library/Application Support/activitywatch");
            (
                support.clone(),
                support,
                env("HOME").join("Library/Logs/activitywatch/aw-tauri"),
            )
        };
        #[cfg(target_os = "linux")]
        let (data, config, log) = {
            let xdg = |var: &str, fallback: &str| {
                std::env::var(var)
                    .ok()
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| env("HOME").join(fallback))
                    .join("activitywatch")
            };
            (
                xdg("XDG_DATA_HOME", ".local/share"),
                xdg("XDG_CONFIG_HOME", ".config"),
                xdg("XDG_CACHE_HOME", ".cache").join("aw-tauri").join("log"),
            )
        };
        // Explicit default profile rather than the getters' AW_PROFILE, so a
        // developer's exported profile cannot fail (or mask) the pins.
        let app = appname_for(DEFAULT_PROFILE);
        // The roots `config_dir_in`/`data_dir_in` build on, joined without
        // calling `module_dir`: on a Windows machine with v0.14.0 data, that
        // would migrate the real install from a test. (The server's own
        // dirs, including `db_path`, are pinned in aw-server-rust.)
        let config_root = aw_server::dirs::user_config_root().unwrap();
        let data_root = aw_server::dirs::user_data_root().unwrap();
        assert_eq!(config_root.join(&app).join(MODULE), config.join("aw-tauri"));
        assert_eq!(data_root.join(&app).join(MODULE), data.join("aw-tauri"));
        assert_eq!(log_dir_in(&app).unwrap(), log);
    }

    /// Fresh, empty scratch dir for building fake install trees.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aw-tauri-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_linux_install_paths_deb_rpm_layout() {
        // /usr/bin/aw-tauri with resources in /usr/lib/aw-tauri/
        let root = scratch_dir("deb");
        let usr = root.join("usr");
        fs::create_dir_all(usr.join("bin")).unwrap();
        fs::create_dir_all(usr.join("lib").join("aw-tauri").join("modules")).unwrap();

        let paths = linux_install_discovery_paths(Some(&usr.join("bin").join("aw-tauri")), None);
        assert_eq!(
            paths,
            vec![
                usr.join("bin"),
                usr.join("lib").join("aw-tauri").join("modules"),
                usr.join("lib").join("aw-tauri"),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_linux_install_paths_without_resources() {
        // No ../lib/aw-tauri next to the binary: only the binary's own dir.
        let root = scratch_dir("bare");
        fs::create_dir_all(root.join("bin")).unwrap();

        let paths = linux_install_discovery_paths(Some(&root.join("bin").join("aw-tauri")), None);
        assert_eq!(paths, vec![root.join("bin")]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_linux_install_paths_appimage_layout() {
        // AppImage: binary runs from a different dir, resources under $APPDIR.
        let root = scratch_dir("appimage");
        let appdir = root.join("squashfs-root");
        let resource = appdir.join("usr").join("lib").join("aw-tauri");
        fs::create_dir_all(resource.join("modules")).unwrap();
        let exe_dir = root.join("elsewhere").join("bin");
        fs::create_dir_all(&exe_dir).unwrap();

        let paths = linux_install_discovery_paths(Some(&exe_dir.join("aw-tauri")), Some(&appdir));
        assert_eq!(
            paths,
            vec![exe_dir.clone(), resource.join("modules"), resource.clone()]
        );

        // When the binary itself lives in $APPDIR/usr/bin, paths aren't duplicated.
        let exe = appdir.join("usr").join("bin").join("aw-tauri");
        let paths = linux_install_discovery_paths(Some(&exe), Some(&appdir));
        assert_eq!(
            paths,
            vec![
                appdir.join("usr").join("bin"),
                resource.join("modules"),
                resource,
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_macos_install_paths_bundle_layout() {
        let root = scratch_dir("macos");
        let contents = root.join("ActivityWatch.app").join("Contents");
        fs::create_dir_all(contents.join("MacOS")).unwrap();
        fs::create_dir_all(contents.join("Resources").join("modules")).unwrap();

        let paths = macos_install_discovery_paths(Some(&contents.join("MacOS").join("aw-tauri")));
        assert_eq!(
            paths,
            vec![
                contents.join("Resources").join("modules"),
                contents.join("Resources"),
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_windows_install_paths_layout() {
        let app = PathBuf::from("ActivityWatch-Tauri");

        // <app>\aw-tauri.exe: modules are in subdirs of the exe's own dir.
        let paths = windows_install_discovery_paths(Some(&app.join("aw-tauri.exe")));
        assert_eq!(paths, vec![app.clone()]);

        // <app>\aw-tauri\aw-tauri.exe: also search the parent install dir.
        let exe_dir = app.join("aw-tauri");
        let paths = windows_install_discovery_paths(Some(&exe_dir.join("aw-tauri.exe")));
        assert_eq!(paths, vec![exe_dir, app]);
    }

    #[test]
    fn test_install_paths_without_exe() {
        assert!(linux_install_discovery_paths(None, None).is_empty());
        assert!(macos_install_discovery_paths(None).is_empty());
        assert!(windows_install_discovery_paths(None).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_discovery_includes_user_paths() {
        if let Ok(home) = std::env::var("HOME") {
            let paths = get_discovery_paths();
            assert!(
                paths.iter().any(|p| p.ends_with("aw-modules")),
                "expected ~/aw-modules in discovery paths, got {:?}",
                paths
            );
            let home_path = PathBuf::from(home);
            assert!(paths.iter().any(|p| p.starts_with(&home_path)));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_discovery_defaults_exclude_install_paths() {
        // Install-relative paths are runtime-only (added by the module manager),
        // never part of the defaults that get written into config.toml. Fake an
        // AppImage mount via APPDIR so there are install paths to leak; the
        // exe's own dir is always one too.
        /// Restores APPDIR and removes the scratch tree even if the test panics.
        struct AppDirGuard(Option<std::ffi::OsString>, PathBuf);
        impl Drop for AppDirGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("APPDIR", v),
                    None => std::env::remove_var("APPDIR"),
                }
                let _ = fs::remove_dir_all(&self.1);
            }
        }

        let root = scratch_dir("defaults-exclude");
        let _guard = AppDirGuard(std::env::var_os("APPDIR"), root.clone());
        let resource = root.join("usr").join("lib").join("aw-tauri");
        fs::create_dir_all(resource.join("modules")).unwrap();
        std::env::set_var("APPDIR", &root);

        let install = get_install_discovery_paths();
        let defaults = get_discovery_paths();

        assert!(
            install.contains(&resource.join("modules")),
            "precondition: APPDIR install paths should be discovered, got {:?}",
            install
        );
        // Only check what came from the fake APPDIR: the exe's own dir is also
        // an install path, and could legitimately equal a user path such as
        // ~/bin when the test binary is run from there.
        for p in install.iter().filter(|p| p.starts_with(&root)) {
            assert!(!defaults.contains(p), "{:?} would be persisted", p);
        }
    }
}
