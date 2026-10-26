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
/// Linux desktop entries have two escaping layers; Windows Run values do not.
pub fn command_executable(command: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    return desktop_command_token(command).map(|(executable, _)| executable);
    #[cfg(not(target_os = "linux"))]
    windows_command_token(command).map(|(executable, _)| executable)
}

#[cfg(any(not(target_os = "linux"), test))]
fn windows_command_token(command: &str) -> Option<(String, usize)> {
    let trimmed = command.trim_start();
    let start = command.len() - trimmed.len();
    let (executable, end) = if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        (rest[..end].to_string(), end + 2)
    } else {
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        (trimmed[..end].to_string(), end)
    };
    if executable.is_empty()
        || end < trimmed.len() && !trimmed[end..].starts_with(char::is_whitespace)
    {
        return None;
    }
    Some((executable, start + end))
}

/// Decode the desktop string layer, then the Exec quoting layer. Keep original
/// byte offsets so replacement never changes the argument suffix.
#[cfg(any(target_os = "linux", test))]
fn desktop_command_token(command: &str) -> Option<(String, usize)> {
    let mut decoded = Vec::new();
    let mut raw = command.char_indices();
    while let Some((offset, ch)) = raw.next() {
        let (ch, end) = if ch == '\\' {
            let (offset, escaped) = raw.next()?;
            let ch = match escaped {
                '\\' => '\\',
                's' => ' ',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => return None,
            };
            (ch, offset + escaped.len_utf8())
        } else {
            (ch, offset + ch.len_utf8())
        };
        decoded.push((ch, end));
    }
    let mut chars = decoded.into_iter().peekable();
    while chars.peek().is_some_and(|(ch, _)| ch.is_whitespace()) {
        chars.next();
    }
    let quoted = chars.peek()?.0 == '"';
    if quoted {
        chars.next();
    }
    let mut executable = String::new();
    let mut end = 0;
    let mut closed = !quoted;
    while let Some((ch, offset)) = chars.next() {
        if quoted && ch == '"' {
            end = offset;
            closed = true;
            if chars.peek().is_some_and(|(ch, _)| !ch.is_whitespace()) {
                return None;
            }
            break;
        }
        if !quoted && ch.is_whitespace() {
            break;
        }
        end = offset;
        let ch = if quoted && ch == '\\' {
            let (escaped, offset) = chars.next()?;
            if !matches!(escaped, '"' | '\\' | '$' | '`') {
                return None;
            }
            end = offset;
            escaped
        } else if ch == '%' {
            // Field codes cannot identify a literal executable path.
            let (escaped, offset) = chars.next()?;
            if escaped != '%' {
                return None;
            }
            end = offset;
            '%'
        } else {
            if !quoted && matches!(ch, '"' | '\\' | '$' | '`' | '\'') {
                return None;
            }
            ch
        };
        executable.push(ch);
    }
    (closed && !executable.is_empty()).then_some((executable, end))
}

#[cfg(any(target_os = "linux", test))]
fn replace_desktop_command(command: &str, expected: &str) -> Option<String> {
    let (executable, end) = desktop_command_token(command)?;
    if !is_activitywatch_executable(&executable)
        || !is_activitywatch_executable(expected)
        || expected.contains(['\n', '\r', '\t'])
    {
        return None;
    }
    let expected = expected
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('$', "\\\\$")
        .replace('`', "\\\\`")
        .replace('%', "%%");
    Some(format!("\"{expected}\"{}", &command[end..]))
}

// Refuse wrappers rather than guessing which later argument is ActivityWatch.
// Split both path separators so the same guard can be regression-tested on Linux.
fn is_activitywatch_executable(executable: &str) -> bool {
    let name = executable.rsplit(['/', '\\']).next().unwrap_or("");
    matches!(name, "aw-tauri" | "aw-tauri.exe" | "aw" | "aw.AppImage")
        || (name.starts_with("aw-") && name.ends_with(".AppImage"))
}

/// Replace only the executable token, retaining the original argument suffix.
#[cfg(any(windows, test))]
fn replace_command_executable(command: &str, expected: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    return replace_desktop_command(command, expected);
    #[cfg(not(target_os = "linux"))]
    replace_windows_command(command, expected)
}

#[cfg(any(windows, test))]
fn replace_windows_command(command: &str, expected: &str) -> Option<String> {
    let (executable, end) = windows_command_token(command)?;
    if !is_activitywatch_executable(&executable)
        || !is_activitywatch_executable(expected)
        || expected.contains(['"', '\n', '\r'])
    {
        return None;
    }
    Some(format!("\"{expected}\"{}", &command[end..]))
}

/// Whether a desktop-entry line is `Key=value`, allowing spaces around '='.
///
/// The [Desktop Entry Spec] permits those spaces; the reader already trims
/// them, so refresh must use the same rule or a moved install stays stale.
///
/// [Desktop Entry Spec]: https://specifications.freedesktop.org/desktop-entry/latest/basic-format.html
fn desktop_line_key_is(line: &str, expected: &str) -> bool {
    line.trim()
        .split_once('=')
        .is_some_and(|(key, _)| key.trim() == expected)
}

#[cfg(any(target_os = "linux", test))]
fn replace_desktop_executable(contents: &str, expected: &str) -> Option<String> {
    let mut offset = 0;
    for line in contents.split_inclusive('\n') {
        if desktop_line_key_is(line, "Exec") {
            let start = offset + line.find('=')? + 1;
            let end = offset + line.trim_end_matches(['\r', '\n']).len();
            let original = &contents[start..end];

            let command = replace_desktop_command(original, expected)?;
            return Some(format!(
                "{}{command}{}",
                &contents[..start],
                &contents[end..]
            ));
        }
        offset += line.len();
    }
    None
}

#[cfg(any(target_os = "macos", test))]
fn replace_plist_executable(contents: &str, expected: &str) -> Option<String> {
    let span = plist_executable_span(contents)?;
    let executable = plist_program_arguments(contents)?;
    if !is_activitywatch_executable(&executable) || !is_activitywatch_executable(expected) {
        return None;
    }
    let escaped = expected
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        // XML normalizes literal CR/CRLF; references retain the path's bytes.
        .replace('\r', "&#13;");
    Some(format!(
        "{}{escaped}{}",
        &contents[..span.start],
        &contents[span.end..]
    ))
}

/// Refresh in place, rather than re-registering and losing user arguments.
pub fn refresh_executable(app_name: &str, expected: &Path) -> std::io::Result<()> {
    let expected = expected
        .to_str()
        .ok_or_else(|| std::io::Error::other("non-UTF-8 executable path"))?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let home =
            dirs::home_dir().ok_or_else(|| std::io::Error::other("home directory unavailable"))?;
        #[cfg(target_os = "linux")]
        let path = home
            .join(".config/autostart")
            .join(format!("{app_name}.desktop"));
        #[cfg(target_os = "macos")]
        let path = home
            .join("Library/LaunchAgents")
            .join(format!("{app_name}.plist"));
        refresh_file(&path, expected)
    }
    #[cfg(windows)]
    {
        use winreg::{
            enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
            RegKey,
        };
        let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
            KEY_READ | KEY_WRITE,
        )?;
        let command: String = key.get_value(app_name)?;
        let updated = replace_command_executable(&command, expected)
            .ok_or_else(|| std::io::Error::other("unrecognized autostart command"))?;
        key.set_value(app_name, &updated)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (app_name, expected);
        Err(std::io::Error::other("unsupported platform"))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn refresh_file(path: &Path, expected: &str) -> std::io::Result<()> {
    let contents = std::fs::read_to_string(path)?;
    #[cfg(target_os = "linux")]
    let updated = if crate::autostart::is_entry_hidden(&contents) {
        None
    } else {
        replace_desktop_executable(&contents, expected)
    };
    #[cfg(target_os = "macos")]
    let updated = replace_plist_executable(&contents, expected);
    let updated =
        updated.ok_or_else(|| std::io::Error::other("unrecognized or disabled autostart entry"))?;
    crate::config_edit::write_atomic(path, updated)
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
        if !desktop_line_key_is(line, "Exec") {
            return None;
        }
        Some(line.split_once('=')?.1.trim().to_string())
    })
}

/// Locate the active first argument using XML events, not tag-shaped text in
/// comments. Reader and writer share the same source span to preserve all other
/// bytes of a user's LaunchAgent.
#[cfg(any(target_os = "macos", test))]
fn plist_executable_span(contents: &str) -> Option<std::ops::Range<usize>> {
    use quick_xml::{events::Event, Reader};

    let mut reader = Reader::from_str(contents);
    let mut found_key = false;
    let mut found_array = false;
    loop {
        match reader.read_event().ok()? {
            Event::Comment(_) => {}
            Event::Text(text) if text.as_ref().bytes().all(|byte| byte.is_ascii_whitespace()) => {}
            Event::Start(tag) if found_array => {
                if tag.name().as_ref() != "string" {
                    return None;
                }
                let span = reader.read_to_end(tag.name()).ok()?;
                let span = span.start as usize..span.end as usize;
                // Nested markup is not a plain executable path.
                if contents[span.clone()].contains('<') {
                    return None;
                }
                return Some(span);
            }
            Event::Start(tag) if found_key => {
                if tag.name().as_ref() != "array" {
                    return None;
                }
                found_array = true;
            }
            Event::Start(tag) if tag.name().as_ref() == "key" => {
                let key = reader.read_text(tag.name()).ok()?;
                found_key = key.as_ref() == "ProgramArguments";
            }
            Event::Eof => return None,
            _ if found_key => return None,
            _ => {}
        }
    }
}

/// The first `<string>` of the `ProgramArguments` array of a launchd plist.
#[cfg(any(target_os = "macos", test))]
fn plist_program_arguments(contents: &str) -> Option<String> {
    let value = &contents[plist_executable_span(contents)?];
    // XML escaping belongs to the file representation, not the filesystem path.
    // Decode once (including numeric references); malformed entities are unknown.
    quick_xml::escape::unescape(value)
        .ok()
        .map(|path| path.into_owned())
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
    fn refreshing_command_preserves_custom_arguments() {
        assert_eq!(
            super::replace_command_executable(
                "\"/old/aw.AppImage\" --mini --port 5700",
                "/new/aw-Activity Watch.AppImage"
            ),
            Some("\"/new/aw-Activity Watch.AppImage\" --mini --port 5700".to_string())
        );
    }

    #[test]
    fn refreshing_supported_renames_remains_refreshable() {
        let first = super::replace_command_executable(
            "/old/aw.AppImage --mini",
            "/new/aw-Activity Watch.AppImage",
        )
        .unwrap();
        assert_eq!(
            super::replace_command_executable(&first, "/next/aw.AppImage"),
            Some("\"/next/aw.AppImage\" --mini".to_string())
        );
        // Do not create a registration that the ownership guard cannot later
        // recognize. Arbitrary renamed images are intentionally unsupported.
        assert_eq!(
            super::replace_command_executable(
                "/old/aw.AppImage --mini",
                "/new/Activity Watch.AppImage"
            ),
            None
        );
        let plist = "<key>ProgramArguments</key><array><string>/old/aw-tauri</string></array>";
        assert_eq!(
            super::replace_plist_executable(plist, "/new/custom-launcher"),
            None
        );
    }

    #[test]
    fn refreshing_desktop_entry_keeps_other_fields_and_arguments() {
        let entry =
            "[Desktop Entry]\r\nName=Custom\r\nExec=/old/aw --mini --port 5700\r\nHidden=false\r\n";
        assert_eq!(
            super::replace_desktop_executable(entry, "/new/aw"),
            Some(entry.replace("Exec=/old/aw", "Exec=\"/new/aw\""))
        );
        assert_eq!(
            super::replace_desktop_executable("[Desktop Entry]\n", "/new/aw"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refreshing_desktop_entry_decodes_escaped_quotes_and_remains_refreshable() {
        let home = temp_home();
        let path = home.join(".config/autostart/aw-tauri.desktop");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let entry = concat!(
            "[Desktop Entry]\nName=Custom\n",
            r#"Exec="/old/A\\"B/aw.AppImage" --mini --port 5700"#,
            "\nHidden=false\n"
        );
        std::fs::write(&path, entry).unwrap();
        assert_eq!(
            super::linux_registered_executable_in(&home, "aw-tauri"),
            Some("/old/A\"B/aw.AppImage".to_string())
        );
        let expected = "/new/A\"B/aw.AppImage";
        super::refresh_file(&path, expected).unwrap();
        assert_eq!(
            super::linux_registered_executable_in(&home, "aw-tauri"),
            Some(expected.to_string())
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            entry.replace("/old/", "/new/")
        );
        super::refresh_file(&path, "/next/aw.AppImage").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            entry.replace(r#"/old/A\\"B/aw.AppImage"#, "/next/aw.AppImage")
        );
    }

    #[test]
    fn desktop_escaping_round_trips_without_touching_arguments() {
        for expected in [
            "/new/A\"B/aw.AppImage",
            "/new/A\\B/aw.AppImage",
            "/new/A$B/aw.AppImage",
            "/new/A`B/aw.AppImage",
            "/new/A%B/aw.AppImage",
            "/new/A'B/aw.AppImage",
            "/new/Å B/aw.AppImage",
        ] {
            let command = super::replace_desktop_command(
                "/old/aw.AppImage --mini --title=\"unchanged\" %u",
                expected,
            )
            .unwrap();
            assert_eq!(
                super::desktop_command_token(&command).map(|(path, _)| path),
                Some(expected.to_string()),
                "{command}"
            );
            assert!(command.ends_with(" --mini --title=\"unchanged\" %u"));
            assert_eq!(
                super::replace_desktop_command(&command, "/next/aw.AppImage"),
                Some("\"/next/aw.AppImage\" --mini --title=\"unchanged\" %u".to_string())
            );
        }
        let windows = r#""C:\new\aw-tauri.exe" --mini --port 5700"#;
        assert_eq!(
            super::windows_command_token(windows),
            Some((r"C:\new\aw-tauri.exe".to_string(), 21))
        );
        assert_eq!(
            super::replace_windows_command(windows, r"C:\next\aw-tauri.exe"),
            Some(r#""C:\next\aw-tauri.exe" --mini --port 5700"#.to_string())
        );
    }

    #[test]
    fn refreshing_desktop_entry_accepts_spaces_around_equals() {
        // The Desktop Entry Spec permits spaces around '='. desktop_exec already
        // reads those keys; refresh must rewrite them too, or a moved AppImage
        // stays registered at the deleted path.
        let entry = "[Desktop Entry]\nName=Custom\nExec = /old/aw.AppImage --mini\nHidden=false\n";
        assert_eq!(
            desktop_exec(entry),
            Some("/old/aw.AppImage --mini".to_string())
        );
        let updated = super::replace_desktop_executable(entry, "/new/aw.AppImage")
            .expect("spec-legal Exec key with spaces around '=' must refresh");
        assert_eq!(
            command_executable(&desktop_exec(&updated).unwrap()),
            Some("/new/aw.AppImage".to_string())
        );
        assert!(updated.contains("--mini"), "{updated}");
        assert!(updated.contains("Name=Custom"), "{updated}");
        assert!(updated.contains("Hidden=false"), "{updated}");
        assert!(!updated.contains("/old/aw.AppImage"), "{updated}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refreshing_file_leaves_wrapped_commands_untouched() {
        let home = temp_home();
        let path = home.join("custom.desktop");
        for command in [
            "env WEBKIT_DISABLE_DMABUF_RENDERER=1 /old/aw.AppImage --mini",
            "/usr/bin/env WEBKIT_DISABLE_DMABUF_RENDERER=1 /old/aw.AppImage --mini",
            "/bin/sh -c '/old/aw.AppImage --mini'",
            "/opt/custom-launcher /old/aw.AppImage --mini",
        ] {
            let entry = format!("[Desktop Entry]\nExec={command}\nHidden=false\n");
            std::fs::write(&path, &entry).unwrap();
            assert!(super::refresh_file(&path, "/new/aw.AppImage").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), entry);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refreshing_file_preserves_arguments_and_disabled_entries() {
        let home = temp_home();
        let path = home.join("custom.desktop");
        let entry = "[Desktop Entry]\nName=Custom\nExec=/old/aw --mini --port 5700\nHidden=false\n";
        std::fs::write(&path, entry).unwrap();
        super::refresh_file(&path, "/new/aw-Activity Watch.AppImage").unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            updated,
            entry.replace("/old/aw", "\"/new/aw-Activity Watch.AppImage\"")
        );
        let disabled = entry.replace("Hidden=false", "Hidden=true");
        std::fs::write(&path, &disabled).unwrap();
        assert!(super::refresh_file(&path, "/new/aw").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), disabled);
        assert!(super::refresh_file(&home.join("missing.desktop"), "/new/aw").is_err());
        assert!(!home.join("missing.desktop").exists());
    }

    #[test]
    fn refreshing_commands_and_plists_refuses_wrappers_on_every_platform() {
        for command in [
            r#""C:\tools\launcher.exe" "C:\old\aw-tauri.exe" --mini"#,
            "/usr/bin/env /old/aw-tauri --mini",
        ] {
            assert_eq!(super::replace_command_executable(command, "/new/aw"), None);
        }
        let plist = "<key>ProgramArguments</key><array><string>/usr/bin/env</string><string>/old/aw-tauri</string></array>";
        assert_eq!(super::replace_plist_executable(plist, "/new/aw"), None);
        let direct = super::replace_windows_command(
            r#""C:\old\aw-tauri.exe" --mini --port 5700"#,
            r"C:\new\aw-tauri.exe",
        )
        .expect("direct Windows launch must be refreshable");
        assert!(direct.ends_with(" --mini --port 5700"));
    }

    #[test]
    fn refreshing_refuses_malformed_commands() {
        assert_eq!(
            super::replace_command_executable("\"/old/aw\"broken --mini", "/new/aw"),
            None
        );
        assert_eq!(
            super::replace_command_executable("\"/old/aw", "/new/aw"),
            None
        );
        assert_eq!(super::replace_command_executable("  ", "/new/aw"), None);
    }

    #[test]
    fn plist_refresh_ignores_commented_out_arguments() {
        let commented =
            "<!-- <key>ProgramArguments</key><array><string>/comment/aw</string></array> -->";
        for between in ["", "<!-- <array><string>/comment/aw</string></array> -->"] {
            let entry = format!("<plist><dict>{commented}<key>ProgramArguments</key>{between}<array><!-- <string>/comment/aw</string> --><string>/old/aw</string><string>--mini</string></array></dict></plist>");
            assert_eq!(plist_program_arguments(&entry), Some("/old/aw".to_string()));
            let refreshed = super::replace_plist_executable(&entry, "/new/aw-tauri")
                .expect("active argument must refresh");
            assert_eq!(refreshed, entry.replace("/old/aw", "/new/aw-tauri"));
            assert_eq!(
                plist_program_arguments(&refreshed),
                Some("/new/aw-tauri".to_string())
            );
        }
        assert_eq!(plist_program_arguments(commented), None);
        assert_eq!(super::replace_plist_executable(commented, "/new/aw"), None);
    }

    #[test]
    fn plist_refresh_refuses_missing_or_non_string_first_arguments() {
        for value in [
            "<array/>",
            "<array></array><string>/old/aw</string>",
            "<array><integer>1</integer><string>/old/aw</string></array>",
            "<array><string>/old/aw</array>",
            "<array><string><!-- /old/aw --></string></array>",
            "<string>/old/aw</string><array><string>/other/aw</string></array>",
        ] {
            let entry = format!("<key>ProgramArguments</key>{value}");
            assert_eq!(plist_program_arguments(&entry), None, "{entry}");
            assert_eq!(super::replace_plist_executable(&entry, "/new/aw"), None);
        }
    }

    #[test]
    fn refreshing_plist_keeps_arguments_and_escapes_path() {
        let entry = "<key>ProgramArguments</key><array><string>/old/aw</string><string>--mini</string></array>";
        assert_eq!(
            super::replace_plist_executable(entry, "/new/A&B/aw-tauri"),
            Some(entry.replace("/old/aw", "/new/A&amp;B/aw-tauri"))
        );
    }

    #[test]
    fn refreshing_plist_preserves_carriage_returns_through_xml_parsing() {
        let entry = "<key>ProgramArguments</key><array><string>/old/aw</string><string>--mini</string></array>";
        for directory in ["A\rB", "A\r\nB", "A&B\rC"] {
            let moved = format!("/Applications/{directory}/aw-tauri");
            let refreshed = super::replace_plist_executable(entry, &moved).expect("refresh");
            assert!(
                !refreshed.contains('\r'),
                "literal carriage returns are normalized by XML parsers"
            );
            assert!(refreshed.contains("&#13;"));
            assert!(refreshed.ends_with("<string>--mini</string></array>"));
            assert_eq!(plist_program_arguments(&refreshed), Some(moved));
        }
    }

    #[test]
    fn plist_entities_decode_before_comparing_and_after_refresh() {
        for (encoded, decoded) in [
            ("A&amp;B", "A&B"),
            ("A&lt;B&gt;C", "A<B>C"),
            ("A&quot;B&apos;C", "A\"B'C"),
            ("A&#38;B&#x26;C", "A&B&C"),
            ("A&amp;lt;B", "A&lt;B"),
            ("A&#x1F980;B", "A🦀B"),
        ] {
            let path = format!("/Applications/{decoded}/aw-tauri");
            let entry = format!("<key>ProgramArguments</key><array><string>/Applications/{encoded}/aw-tauri</string><string>--mini</string></array>");
            let executable = plist_program_arguments(&entry).expect("decoded path");
            assert!(executable_matches_path(&executable, Path::new(&path)));

            let moved = format!("/Moved/{decoded}/aw-tauri");
            let refreshed = super::replace_plist_executable(&entry, &moved).expect("refresh");
            assert!(refreshed.ends_with("<string>--mini</string></array>"));
            assert_eq!(plist_program_arguments(&refreshed), Some(moved));
        }
    }

    #[test]
    fn plist_refresh_checks_the_decoded_executable_name() {
        let entry = "<key>ProgramArguments</key><array><string>/old/&#97;w-tauri</string><string>--mini</string></array>";
        let refreshed = super::replace_plist_executable(entry, "/new/aw-tauri").expect("refresh");
        assert_eq!(
            refreshed,
            entry.replace("/old/&#97;w-tauri", "/new/aw-tauri")
        );
        for invalid in ["A&unknown;B", "A&amp", "A&#x110000;B"] {
            let malformed = entry.replace("/old/&#97;w-tauri", &format!("/{invalid}/aw-tauri"));
            assert_eq!(plist_program_arguments(&malformed), None);
            assert_eq!(
                super::replace_plist_executable(&malformed, "/new/aw-tauri"),
                None
            );
        }
    }

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
        let home = temp_home();
        let expected = home.join("aw-tauri");
        let other = home.join("aw-tauri-old");
        std::fs::write(&expected, "current").unwrap();
        std::fs::write(&other, "old").unwrap();
        assert!(executable_matches_path(
            expected.to_str().unwrap(),
            &expected
        ));
        assert!(!executable_matches_path(other.to_str().unwrap(), &expected));
        assert!(!executable_matches_path("", &expected));
        #[cfg(unix)]
        {
            let link = home.join("aw-tauri-link");
            std::os::unix::fs::symlink(&expected, &link).unwrap();
            assert!(executable_matches_path(link.to_str().unwrap(), &expected));
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn executable_matches_path_detects_a_moved_install() {
        // The old AppImage path is gone, so only a literal comparison can
        // decide — and it must report a mismatch.
        let home = temp_home();
        let expected = home.join("aw-v0.14.2.AppImage");
        let old = home.join("aw-v0.14.0.AppImage");
        std::fs::write(&expected, "current").unwrap();
        assert!(!old.exists());
        assert!(!executable_matches_path(old.to_str().unwrap(), &expected));
        std::fs::remove_dir_all(home).unwrap();
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
