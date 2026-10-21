//! Reading the OS autostart entry, so a moved install can refresh it.
//!
//! The autostart entry records the executable path that was current when it
//! was created. Linux AppImage users (and anyone who moves the install)
//! end up with an entry that runs a path which no longer exists: login boots
//! nothing, and the app has no way to tell the user why.
//!
//! These helpers read the registered executable and compare it with the path
//! the app *would* register right now, so
//! [`crate::autostart::refresh_registered_path`] can rewrite the entry in
//! place.

use std::path::{Path, PathBuf};

/// The executable path the autostart entry should point at.
///
/// This mirrors what `tauri-plugin-autostart` registers: on Linux an AppImage
/// run registers the image path (`$APPIMAGE`), not the extracted binary
/// inside the mount point, so comparing against `current_exe()` alone would
/// report a mismatch for every AppImage install.
///
/// It is also the path worth showing the user: for an AppImage run,
/// `current_exe()` is a temporary mount (`/tmp/.mount_*/aw-tauri`) that says
/// nothing about which build is running.
pub fn expected_registered_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let appimage = std::env::var_os("APPIMAGE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    #[cfg(not(target_os = "linux"))]
    let appimage: Option<PathBuf> = None;

    expected_registered_path_with(appimage, std::env::current_exe().ok())
}

/// The pure form of [`expected_registered_path`], with the environment read by
/// the caller so it can be tested.
pub fn expected_registered_path_with(
    appimage: Option<PathBuf>,
    current_exe: Option<PathBuf>,
) -> Option<PathBuf> {
    appimage.or(current_exe)
}

/// The executable the OS autostart entry runs, as a path string.
///
/// `None` means "unknown", not "not registered": an unsupported platform, an
/// unreadable file, or an entry the user disabled in place. Callers must not
/// treat that as a mismatch.
pub fn registered_executable(app_name: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux_registered_executable(app_name)
    }
    #[cfg(target_os = "macos")]
    {
        macos_registered_executable(app_name)
    }
    #[cfg(windows)]
    {
        windows_registered_executable(app_name)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = app_name;
        None
    }
}

/// The executable token of a command line.
///
/// Handles the quoting used by desktop entries (`Exec="/path with spaces/x"`).
pub fn command_executable(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        return rest.split('"').next().map(|s| s.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        return rest.split('\'').next().map(|s| s.to_string());
    }
    trimmed.split_whitespace().next().map(|s| s.to_string())
}

/// Whether the executable `executable` is the one at `expected`.
///
/// Paths are compared literally first, then canonically (so a symlinked
/// install still matches). A path that no longer exists — the stale AppImage
/// this whole module exists for — canonicalizes to an error, and the literal
/// comparison already failed, so it counts as a mismatch.
pub fn executable_matches_path(executable: &str, expected: &Path) -> bool {
    let executable = Path::new(executable);
    if executable == expected {
        return true;
    }
    match (executable.canonicalize(), expected.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The `Exec=` value of an XDG desktop entry.
fn desktop_exec(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once('=')?;
        if key.trim() == "Exec" {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

/// The first `<string>` of the `ProgramArguments` array of a launchd plist.
#[cfg(any(target_os = "macos", test))]
fn plist_program_arguments(contents: &str) -> Option<String> {
    let array = contents.split_once("<key>ProgramArguments</key>")?.1;
    let array = array.split_once("<array>")?.1;
    let array = array.split_once("</array>")?.0;
    let first = array.split_once("<string>")?.1;
    let value = first.split_once("</string>")?.0;
    Some(value.trim().to_string())
}

#[cfg(target_os = "linux")]
fn linux_registered_executable(app_name: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    linux_registered_executable_in(&home, app_name)
}

/// Read the executable of the XDG autostart entry of `app_name` below `home`.
#[cfg(target_os = "linux")]
pub fn linux_registered_executable_in(home: &Path, app_name: &str) -> Option<String> {
    let path = home
        .join(".config/autostart")
        .join(format!("{app_name}.desktop"));
    let contents = std::fs::read_to_string(path).ok()?;
    // A hidden entry is one the user disabled in the desktop environment; it
    // must not be rewritten, or the refresh would silently re-enable it.
    if crate::autostart::is_entry_hidden(&contents) {
        return None;
    }
    // The `Exec=` line is a command line: the executable comes first.
    command_executable(&desktop_exec(&contents)?)
}

#[cfg(target_os = "macos")]
fn macos_registered_executable(app_name: &str) -> Option<String> {
    // Only the LaunchAgent launcher writes a readable file. The default
    // profile uses AppleScript login items, which cannot be read back, so
    // there is nothing to compare and the entry is left alone.
    let path = dirs::home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{app_name}.plist"));
    let contents = std::fs::read_to_string(path).ok()?;
    // `ProgramArguments[0]` is the executable itself, not a command line, so
    // it is used verbatim: splitting it would break on a path with spaces.
    plist_program_arguments(&contents)
}

#[cfg(windows)]
fn windows_registered_executable(app_name: &str) -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run")
        .ok()?;
    // The `Run` value is a command line: the executable comes first.
    command_executable(&key.get_value::<String, _>(app_name).ok()?)
}

#[cfg(test)]
mod tests {
    use super::{
        command_executable, desktop_exec, executable_matches_path, expected_registered_path_with,
        plist_program_arguments,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn desktop_exec_reads_the_exec_line() {
        let entry =
            "[Desktop Entry]\nType=Application\nName=aw-tauri\nExec=/opt/aw-tauri --profile x\n";
        assert_eq!(
            desktop_exec(entry),
            Some("/opt/aw-tauri --profile x".to_string())
        );
    }

    #[test]
    fn desktop_exec_ignores_other_keys() {
        let entry = "[Desktop Entry]\nName=aw-tauri\nExec=/opt/aw-tauri\n";
        assert_eq!(desktop_exec(entry), Some("/opt/aw-tauri".to_string()));
        assert_eq!(desktop_exec("[Desktop Entry]\nName=aw-tauri\n"), None);
    }

    #[test]
    fn command_executable_takes_the_first_token() {
        assert_eq!(
            command_executable("/opt/aw-tauri --profile research"),
            Some("/opt/aw-tauri".to_string())
        );
        assert_eq!(command_executable("   "), None);
    }

    #[test]
    fn command_executable_handles_quoted_paths() {
        assert_eq!(
            command_executable("\"/opt/Activity Watch/aw-tauri\" --hidden"),
            Some("/opt/Activity Watch/aw-tauri".to_string())
        );
    }

    #[test]
    fn executable_matches_path_compares_the_executable_only() {
        let expected = Path::new("/opt/aw-tauri");
        assert!(executable_matches_path("/opt/aw-tauri", expected));
        assert!(!executable_matches_path("/opt/aw-tauri-old", expected));
        assert!(!executable_matches_path("", expected));
    }

    #[test]
    fn executable_matches_path_detects_a_moved_install() {
        // The old AppImage path is gone, so only a literal comparison can
        // decide — and it must report a mismatch.
        let expected = Path::new("/home/user/Downloads/aw-v0.14.2.AppImage");
        assert!(!executable_matches_path(
            "/home/user/Downloads/aw-v0.14.0.AppImage",
            expected
        ));
    }

    #[test]
    fn expected_path_prefers_the_appimage_over_the_mounted_binary() {
        // An AppImage run must register the image path, not the extracted
        // binary inside the mount point, or every launch looks like a move.
        let expected = expected_registered_path_with(
            Some(PathBuf::from("/home/user/aw-v0.14.2.AppImage")),
            Some(PathBuf::from("/tmp/.mount_abc/aw-tauri")),
        );
        assert_eq!(
            expected,
            Some(PathBuf::from("/home/user/aw-v0.14.2.AppImage"))
        );

        // Without an AppImage the executable itself is registered.
        let expected =
            expected_registered_path_with(None, Some(PathBuf::from("/usr/bin/aw-tauri")));
        assert_eq!(expected, Some(PathBuf::from("/usr/bin/aw-tauri")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_entry_is_read_and_detected() {
        // A moved AppImage: the entry still runs the old image while the build
        // now is the new one, so the entry has to be rewritten.
        let home = temp_home();
        let dir = home.join(".config/autostart");
        std::fs::create_dir_all(&dir).expect("create autostart dir");
        std::fs::write(
            dir.join("aw-tauri.desktop"),
            "[Desktop Entry]\nType=Application\nName=aw-tauri\nExec=/home/user/aw-v0.14.0.AppImage\n",
        )
        .expect("write desktop entry");

        let executable =
            super::linux_registered_executable_in(&home, "aw-tauri").expect("entry should be read");
        assert_eq!(executable, "/home/user/aw-v0.14.0.AppImage");
        let expected = PathBuf::from("/home/user/aw-v0.14.2.AppImage");
        assert!(!executable_matches_path(&executable, &expected));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_hidden_entry_is_not_read() {
        // A hidden entry is one the user disabled in place: it must not be
        // rewritten, or the refresh would re-enable it.
        let home = temp_home();
        let dir = home.join(".config/autostart");
        std::fs::create_dir_all(&dir).expect("create autostart dir");
        std::fs::write(
            dir.join("aw-tauri.desktop"),
            "[Desktop Entry]\nType=Application\nName=aw-tauri\nExec=/opt/aw-tauri\nHidden=true\n",
        )
        .expect("write desktop entry");
        assert_eq!(
            super::linux_registered_executable_in(&home, "aw-tauri"),
            None
        );
    }

    #[test]
    fn plist_program_arguments_reads_the_first_argument() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>aw-tauri</string>
    <key>ProgramArguments</key>
    <array><string>/Applications/aw-tauri.app/Contents/MacOS/aw-tauri</string><string>--profile</string></array>
    <key>RunAtLoad</key>
    <true/>
  </dict>
</plist>"#;
        assert_eq!(
            plist_program_arguments(plist),
            Some("/Applications/aw-tauri.app/Contents/MacOS/aw-tauri".to_string())
        );
    }

    #[test]
    fn a_plist_path_with_spaces_is_kept_whole() {
        // `ProgramArguments[0]` is a path, not a command line: splitting it
        // would make every install with a space in its path look moved.
        let plist = "<key>ProgramArguments</key><array><string>/Applications/Activity Watch/aw-tauri</string></array>";
        let executable = plist_program_arguments(plist).expect("path");
        assert_eq!(executable, "/Applications/Activity Watch/aw-tauri");
        assert!(executable_matches_path(
            &executable,
            Path::new("/Applications/Activity Watch/aw-tauri")
        ));
    }

    #[cfg(target_os = "linux")]
    fn temp_home() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aw-tauri-autostart-entry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp home");
        path
    }
}
