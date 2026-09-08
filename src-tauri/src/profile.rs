//! Named instance profiles for isolated ActivityWatch runs.
//!
//! A *profile* names an isolated instance (data, config, logs, lockfile).
//! `default` is the ordinary install, `testing` is what `--testing` has always
//! meant, and any other name (for example `research`) is a sibling instance
//! that can run at the same time as the others.
//!
//! aw-tauri is the launcher, so its job is small: resolve the profile, export
//! it as `AW_PROFILE` for the modules it spawns, and use it for the things
//! aw-tauri itself owns (dirs via [`crate::dirs`], single-instance id, tray
//! label). Spawned modules inherit the env var without every CLI growing a flag.
//!
//! Same validation rule as aw-server-rust and aw-qt: lowercase alphanumeric
//! plus `-`/`_`, at most 32 chars, so a profile is always a safe path segment.

pub const DEFAULT_PROFILE: &str = "default";
pub const TESTING_PROFILE: &str = "testing";
pub const ENV_VAR: &str = "AW_PROFILE";

/// Return `Ok(())` if `name` is a usable profile, or `Err(message)` if not.
pub fn validate_profile(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("profile name must not be empty".into());
    }
    if name.len() > 32 {
        return Err(format!(
            "profile name too long ({} chars, max 32)",
            name.len()
        ));
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphabetic() {
        return Err(format!(
            "profile name must start with a letter, got '{first}'"
        ));
    }
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() && c != '-' && c != '_' {
            return Err(format!("invalid character '{c}' in profile name"));
        }
    }
    if name != name.to_lowercase() {
        return Err("profile name must be lowercase".into());
    }
    Ok(())
}

/// Resolve the effective profile from CLI flags and an optional env value.
///
/// `--testing` is an alias for `--profile testing`; passing both is only an
/// error if they disagree. `--profile` wins over `AW_PROFILE`. Invalid names
/// are rejected rather than silently ignored.
pub fn resolve_profile(cli_profile: Option<&str>, testing: bool) -> Result<String, String> {
    resolve_profile_from(
        cli_profile,
        testing,
        std::env::var(ENV_VAR).ok().filter(|s| !s.is_empty()),
    )
}

fn resolve_profile_from(
    cli_profile: Option<&str>,
    testing: bool,
    env_profile: Option<String>,
) -> Result<String, String> {
    if let Some(name) = cli_profile {
        validate_profile(name)?;
        if testing && name != TESTING_PROFILE {
            return Err(format!(
                "--testing conflicts with --profile {name}: --testing is an alias for --profile {TESTING_PROFILE}"
            ));
        }
        return Ok(name.to_string());
    }
    if testing {
        return Ok(TESTING_PROFILE.to_string());
    }
    match env_profile {
        Some(name) => {
            validate_profile(&name)?;
            Ok(name)
        }
        None => Ok(DEFAULT_PROFILE.to_string()),
    }
}

pub fn is_testing(profile: &str) -> bool {
    profile == TESTING_PROFILE
}

pub fn is_default(profile: &str) -> bool {
    profile == DEFAULT_PROFILE
}

/// Publish the profile to this process and its children.
pub fn export_profile(profile: &str) {
    std::env::set_var(ENV_VAR, profile);
}

/// Profile currently exported (or `"default"` if unset/invalid).
pub fn current_profile() -> String {
    match std::env::var(ENV_VAR) {
        Ok(name) if validate_profile(&name).is_ok() => name,
        _ => DEFAULT_PROFILE.to_string(),
    }
}

/// Single-instance lock filename. `default` keeps the legacy name so existing
/// watchers still fire; other profiles get a suffix so they don't steal the
/// default instance's focus signal (they may share a runtime dir with it).
pub fn lockfile_name(profile: &str) -> String {
    if is_default(profile) {
        "single_instance.lock".to_string()
    } else {
        format!("single_instance-{profile}.lock")
    }
}

pub fn tray_tooltip(profile: &str) -> String {
    if is_default(profile) {
        "ActivityWatch".to_string()
    } else {
        format!("ActivityWatch ({profile})")
    }
}

pub fn window_title(profile: &str) -> String {
    if is_default(profile) {
        "aw-tauri".to_string()
    } else {
        format!("aw-tauri ({profile})")
    }
}

/// OS autostart entry name. The default profile keeps the plugin's legacy
/// package-name identity; named profiles use distinct identities so one tray
/// toggle cannot overwrite or disable another profile's entry.
pub fn autostart_app_name(profile: &str) -> Option<String> {
    if is_default(profile) {
        None
    } else {
        Some(format!("aw-tauri-{profile}"))
    }
}

/// Linux D-Bus well-known name base for the single-instance plugin.
/// `default` uses the bundle identifier; other profiles get a suffix so they
/// can run at the same time as the default instance.
pub fn single_instance_dbus_id(profile: &str) -> String {
    if is_default(profile) {
        "net.activitywatch.app".to_string()
    } else {
        format!("net.activitywatch.app.{profile}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_profile() {
        assert!(validate_profile("default").is_ok());
        assert!(validate_profile("testing").is_ok());
        assert!(validate_profile("research").is_ok());
        assert!(validate_profile("my-profile").is_ok());
        assert!(validate_profile("profile_1").is_ok());

        assert!(validate_profile("").is_err());
        assert!(
            validate_profile("Research").is_err(),
            "uppercase should be rejected"
        );
        assert!(
            validate_profile("-bad").is_err(),
            "must start with a letter"
        );
        assert!(
            validate_profile("1work").is_err(),
            "digit-leading profiles produce invalid D-Bus names"
        );
        assert!(validate_profile("bad name").is_err(), "spaces not allowed");
        assert!(
            validate_profile("a/b").is_err(),
            "path separator not allowed"
        );
        assert!(validate_profile(&"a".repeat(33)).is_err(), "too long");
    }

    #[test]
    fn test_resolve_profile() {
        assert_eq!(
            resolve_profile_from(None, false, None).unwrap(),
            DEFAULT_PROFILE
        );
        assert_eq!(
            resolve_profile_from(None, true, None).unwrap(),
            TESTING_PROFILE
        );
        assert_eq!(
            resolve_profile_from(Some("research"), false, None).unwrap(),
            "research"
        );
        assert_eq!(
            resolve_profile_from(Some("testing"), true, None).unwrap(),
            TESTING_PROFILE
        );
        assert!(resolve_profile_from(Some("research"), true, None).is_err());
        assert_eq!(
            resolve_profile_from(None, false, Some("research".into())).unwrap(),
            "research"
        );
        // --profile wins over AW_PROFILE
        assert_eq!(
            resolve_profile_from(Some("research"), false, Some("other".into())).unwrap(),
            "research"
        );
        // --testing wins over AW_PROFILE (it's an explicit CLI alias)
        assert_eq!(
            resolve_profile_from(None, true, Some("research".into())).unwrap(),
            TESTING_PROFILE
        );
        assert!(resolve_profile_from(Some("Research"), false, None).is_err());
        assert!(resolve_profile_from(None, false, Some("Not Valid".into())).is_err());
    }

    #[test]
    fn test_lockfile_and_labels() {
        assert_eq!(lockfile_name(DEFAULT_PROFILE), "single_instance.lock");
        assert_eq!(
            lockfile_name(TESTING_PROFILE),
            "single_instance-testing.lock"
        );
        assert_eq!(lockfile_name("research"), "single_instance-research.lock");
        assert_eq!(tray_tooltip(DEFAULT_PROFILE), "ActivityWatch");
        assert_eq!(tray_tooltip("research"), "ActivityWatch (research)");
        assert_eq!(window_title(DEFAULT_PROFILE), "aw-tauri");
        assert_eq!(window_title("research"), "aw-tauri (research)");
        assert_eq!(autostart_app_name(DEFAULT_PROFILE), None);
        assert_eq!(
            autostart_app_name("research"),
            Some("aw-tauri-research".to_string())
        );
        assert_eq!(
            single_instance_dbus_id(DEFAULT_PROFILE),
            "net.activitywatch.app"
        );
        assert_eq!(
            single_instance_dbus_id("research"),
            "net.activitywatch.app.research"
        );
    }
}
