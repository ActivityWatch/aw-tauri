# Autostart Detection Spec

This document defines the per-OS locations aw-tauri and aw-qt must probe to
determine whether ActivityWatch is registered to start at login, who owns that
registration, and what the "Start at login" toggle should show and do in each
case.

Applies to: ActivityWatch/aw-tauri#278, ActivityWatch/aw-qt#136.

---

## Goals

1. **Detect all autostart entries**, not only the one the app writes itself.
2. **Never create a second entry** if someone else already manages one.
3. **Surface the owner** — show "managed by \<source\>" when an entry was not
   written by us, so the user knows where to go to change it.
4. **Never fight the OS** — don't reapply from config on every launch; read OS
   state back and let it win.

---

## Terminology

| Term | Meaning |
|---|---|
| **Own entry** | An autostart entry written by aw-tauri / aw-qt itself (the app manages it). |
| **External entry** | An entry written by an installer, package manager, or the user directly outside the app. The app detects it but does not manage it. |
| **Managed source** | The human-readable label for what wrote an external entry (e.g. "the installer", "a .deb package", "a systemd unit"). |

---

## Per-OS Detection Locations

### Linux

Probe these locations in priority order.  Stop after the first match.

| Priority | Location | Type | Notes |
|---|---|---|---|
| 1 | `$XDG_CONFIG_DIRS/autostart/*.desktop` (usually `/etc/xdg/autostart/`) | External | System-wide; `Exec` must target our binary. `Hidden=true` means disabled. |
| 2 | `~/.config/autostart/<app>.desktop` (user-level XDG) | Own / External | Own if written by us; external if e.g. installed by a package. Match by `Exec` path. |
| 3 | `systemctl --user is-enabled <unit>` | External | `<unit>` = `aw-qt.service` or `aw-tauri.service` as appropriate. `enabled` / `enabled-runtime` = on. |

**Exec matching rule**: the desktop `Exec` line targets us if it resolves to our
binary path.  For AppImage installs use `$APPIMAGE` (set by the AppImage
runtime) as the stable comparison path, not the ephemeral fuse-mount path.
For `.deb`/AUR installs compare against the installed path
(`/usr/bin/activitywatch` or `/usr/bin/aw-qt`).

**Writing**: we only manage the user-level XDG desktop file
(`~/.config/autostart/<app>.desktop`).  We never write to system-wide paths.
If a system-wide file says `Hidden=true` we are free to create/update the
user-level override.

### macOS

| Priority | Location | Type | Notes |
|---|---|---|---|
| 1 | Login Items (System Events) | Own / External | Current write target since aw-tauri b7d5832. Query via `osascript`. |
| 2 | `~/Library/LaunchAgents/aw-tauri.plist` | Legacy own | Written by aw-tauri before b7d5832. Migrate (remove) on first run if default-profile plist. |
| 3 | `~/Library/LaunchAgents/<profile>.plist` | Legacy own | Named-profile leftovers; already handled by `migrate_legacy_named_profile_entry`. |
| 4 | `/Library/LaunchAgents/aw-tauri.plist` | External | System-wide; treat as external (read-only hint). |

**Writing**: we only manage Login Items (AppleScript launcher).  We do not
write LaunchAgent plists for new installs.

### Windows

| Priority | Location | Type | Notes |
|---|---|---|---|
| 1 | `HKCU\…\Run\<app-name>` | Own / External | Own if we wrote the value; external if e.g. an NSIS installer did. Identify by target path. |
| 2 | `HKCU\…\StartupApproved\Run\<app-name>` | OS-managed state | Present when Task Manager or the OS disabled the Run entry. Detect alongside the Run key. |
| 3 | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\<name>.lnk` (per-user startup folder) | External | An NSIS installer shortcut. Resolve the .lnk target to identify it as ours. |
| 4 | `%ProgramData%\Microsoft\Windows\Start Menu\Programs\Startup\<name>.lnk` (common startup folder) | External | System-wide installer shortcut. Treat as external (read-only hint). |

**Identifying "ours"**: a `Run` value is our own entry if we wrote it (stored in
the value's data path pointing at our binary).  Use the binary path to
distinguish entries we manage from entries an installer wrote.

**Writing**: we manage only our own `Run` registry value.  We do not write or
delete installer shortcuts — those belong to the installer/uninstaller.

---

## Decision Table

Given the result of the full detection scan:

| Detected state | Toggle shows | Toggle enable does | Toggle disable does |
|---|---|---|---|
| Own entry, OS-enabled | ✅ ON | — (already on) | Remove / disable own entry |
| Own entry, OS-disabled (e.g. `StartupApproved` cleared by Task Manager) | ✅ ON (dimmed / "disabled by OS") | Re-enable own entry in OS | Remove own entry |
| External entry, enabled | ✅ ON — "managed by \<source\>" | No-op, show tooltip | No-op, show where to disable |
| External entry, disabled | ○ OFF — "managed by \<source\>" | No-op, show tooltip | No-op |
| No entry anywhere | ○ OFF | Create own entry | — (already off) |
| Both own + external (transition state) | ✅ ON | — | Disable own entry only |

**"managed by \<source\>"** text examples:
- "Managed by the installer — remove via Add/Remove Programs to change this setting."
- "Managed by a system-wide package — use `systemctl --user disable aw-qt` to turn this off."
- "Managed by a Login Item added outside the app."

---

## Return-type sketch (aw-tauri)

The Rust detection function should return a richer type than `bool`:

```rust
pub enum AutostartState {
    /// We own the entry and the OS has it enabled.
    OwnEnabled,
    /// We own the entry but the OS has it disabled (e.g. Task Manager disabled it).
    OwnDisabled,
    /// An external entry exists; we should not create a second one.
    ExternalManaged { source: String },
    /// No entry found anywhere.
    Off,
}
```

`is_registered()` becomes `detect_state() -> Result<AutostartState, String>`.
The tray item and `sync_from_config` use this to decide both what to display and
whether it's safe to write.

For aw-qt (Python) the equivalent is a `(state: str, source: str | None)` tuple
returned from `_detect_state()`.

---

## Test cases

One test per location type.  These apply to both aw-tauri and aw-qt
implementations.

| ID | Setup | Expected state | Expected toggle label |
|---|---|---|---|
| L1 | User-level `~/.config/autostart/aw-qt.desktop` exists, `Hidden=false` | `OwnEnabled` | ✅ ON |
| L2 | User-level desktop file exists, `Hidden=true` | `OwnDisabled` | ✅ ON (disabled) |
| L3 | Only `/etc/xdg/autostart/aw-qt.desktop` exists, `Hidden=false` | `ExternalManaged("system package")` | ✅ ON – managed by system |
| L4 | Only systemd unit `aw-qt.service` is enabled | `ExternalManaged("systemd unit")` | ✅ ON – managed by systemd |
| L5 | AppImage: `Exec=$APPIMAGE`, desktop file matches | `OwnEnabled` | ✅ ON |
| M1 | Login Item exists (AppleScript) | `OwnEnabled` | ✅ ON |
| M2 | Login Item absent; legacy `~/Library/LaunchAgents/aw-tauri.plist` present (default profile) | `OwnEnabled` (migrated on startup) | ✅ ON |
| W1 | `HKCU\…\Run\aw-tauri` set, no `StartupApproved` entry | `OwnEnabled` | ✅ ON |
| W2 | `HKCU\…\Run\aw-tauri` set, `StartupApproved` value disables it | `OwnDisabled` | ✅ ON (disabled) |
| W3 | Only `%APPDATA%\…\Startup\ActivityWatch.lnk` exists (no Run key) | `ExternalManaged("installer")` | ✅ ON – managed by installer |
| ALL | No entry in any location | `Off` | ○ OFF |

---

## Implementation order

1. **aw-tauri** — implement `detect_state()` returning `AutostartState`; update
   `sync_from_config` and the tray item to consume it.  Full per-OS coverage.
2. **aw-qt** — implement `_detect_state()` in Python for Linux (XDG + systemd)
   and macOS (Login Items detection via `osascript`).  Windows detection in a
   follow-on PR (requires a Windows test environment).
3. Both: surface "managed by \<source\>" in the UI when `ExternalManaged` is
   detected — read-only toggle with a tooltip.

The Linux items in aw-qt (XDG system-wide detection + AppImage `Exec` fix) are
the highest-priority slice: they cover users who installed via `.deb`, AUR, or
AppImage and now have both an installer-created entry and an app-created one.
