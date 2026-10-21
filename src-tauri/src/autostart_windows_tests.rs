//! Registry integration fixtures use unique test-owned values, never aw-tauri's
//! real Run value. They exercise the plugin dependency's StartupApproved reads.

use super::migrate_legacy_windows_run_value;
use std::sync::atomic::{AtomicU64, Ordering};
use winreg::enums::{RegType::REG_BINARY, HKEY_CURRENT_USER};
use winreg::{RegKey, RegValue};

const ENABLED: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
// auto-launch checks the last eight bytes (a disabled timestamp), not just byte 0.
const DISABLED: [u8; 12] = [3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
const COMMAND: &str = r#""C:\aw-tauri-test.exe" --profile research"#;

struct LegacyRun {
    name: String,
    run: RegKey,
    approval: RegKey,
}

impl LegacyRun {
    fn new(command: &str, approval: &[u8]) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu
            .create_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run")
            .unwrap();
        let (approved, _) = hkcu
            .create_subkey(
                r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
            )
            .unwrap();
        let fixture = Self {
            name: format!(
                "aw-tauri-migration-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            run,
            approval: approved,
        };
        fixture.run.set_value(&fixture.name, &command).unwrap();
        fixture
            .approval
            .set_raw_value(
                &fixture.name,
                &RegValue {
                    vtype: REG_BINARY,
                    bytes: approval.to_vec(),
                },
            )
            .unwrap();
        fixture
    }

    fn command(&self) -> String {
        self.run.get_value(&self.name).unwrap()
    }

    fn approval(&self) -> Vec<u8> {
        self.approval.get_raw_value(&self.name).unwrap().bytes
    }
}

impl Drop for LegacyRun {
    fn drop(&mut self) {
        let _ = self.run.delete_value(&self.name);
        let _ = self.approval.delete_value(&self.name);
    }
}

#[test]
fn windows_migration_preserves_startupapproved_disabled_intent() {
    let legacy = LegacyRun::new(COMMAND, &DISABLED);
    assert!(
        !auto_launch::AutoLaunch::new(&legacy.name, "", &[] as &[&str])
            .is_enabled()
            .unwrap()
    );
    let mut requested = Vec::new();
    let result = migrate_legacy_windows_run_value("research", &legacy.name, &mut |enabled| {
        assert_eq!(legacy.command(), COMMAND, "prepare before removal");
        requested.push(enabled);
        Ok(())
    })
    .unwrap();
    assert_eq!(result, Some(false));
    assert_eq!(requested, vec![false], "replacement must stay disabled");
    assert!(legacy.run.get_raw_value(&legacy.name).is_err());
    assert_eq!(legacy.approval(), DISABLED);
}

#[test]
fn windows_migration_failure_retains_original_for_successful_retry() {
    let legacy = LegacyRun::new(COMMAND, &ENABLED);
    let result = migrate_legacy_windows_run_value("research", &legacy.name, &mut |enabled| {
        assert!(enabled);
        assert_eq!(legacy.command(), COMMAND);
        Err("registration failed".into())
    });
    assert!(result.is_err());
    assert_eq!(legacy.command(), COMMAND);
    assert_eq!(legacy.approval(), ENABLED);

    let mut called = false;
    let result = migrate_legacy_windows_run_value("research", &legacy.name, &mut |enabled| {
        assert!(enabled);
        assert_eq!(legacy.command(), COMMAND);
        called = true;
        Ok(())
    })
    .unwrap();
    assert!(called);
    assert_eq!(result, Some(true));
    assert!(legacy.run.get_raw_value(&legacy.name).is_err());
}

#[test]
fn windows_migration_leaves_another_profiles_values_untouched() {
    let legacy = LegacyRun::new(COMMAND, &DISABLED);
    let result = migrate_legacy_windows_run_value("other", &legacy.name, &mut |_| {
        panic!("must not prepare another profile's replacement")
    })
    .unwrap();
    assert_eq!(result, None);
    assert_eq!(legacy.command(), COMMAND);
    assert_eq!(legacy.approval(), DISABLED);
}

#[test]
fn windows_migration_ignores_absent_run_value() {
    let legacy = LegacyRun::new(COMMAND, &DISABLED);
    legacy.run.delete_value(&legacy.name).unwrap();
    let result = migrate_legacy_windows_run_value("research", &legacy.name, &mut |_| {
        panic!("must not prepare absent entry")
    })
    .unwrap();
    assert_eq!(result, None);
    assert_eq!(legacy.approval(), DISABLED);
}
