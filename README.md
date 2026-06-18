# aw-tauri

[![Build](https://github.com/ActivityWatch/aw-tauri/actions/workflows/build.yml/badge.svg)](https://github.com/ActivityWatch/aw-tauri/actions/workflows/build.yml)

The next-gen module manager and desktop shell for [ActivityWatch](https://activitywatch.net/), built with [Tauri](https://tauri.app/). Replaces the legacy [aw-qt](https://github.com/ActivityWatch/aw-qt) (Python/Qt).

## Why aw-tauri?

- **Simpler builds** — No more PyInstaller. `npx tauri build` produces deb, rpm, AppImage, .app, and MSI from one codebase.
- **Embedded server** — Runs aw-server-rust in-process; no separate server binary needed.
- **Modern stack** — Rust + WebView instead of Python + Qt, with native code-signing and updater support.
- **Cross-platform** — Builds for Linux, macOS, and Windows including ARM64.
- **Mobile future** — Tauri supports iOS and Android, enabling future mobile ActivityWatch builds.

## What it does

aw-tauri bundles everything needed to run ActivityWatch into a single application:

- **Embedded server** — Runs `aw-server-rust` in-process (no separate server binary needed)
- **Module manager** — Discovers, starts, stops, and monitors watcher processes (aw-watcher-afk, aw-watcher-window, etc.)
- **System tray** — Tray icon with menu to open the dashboard, toggle modules, and access config/log folders
- **WebView dashboard** — Opens the aw-webui dashboard in a native window
- **Crash recovery** — Exponential backoff restart for crashed modules (up to 3 retries)
- **Single instance** — Only one aw-tauri process runs at a time via lockfile detection
- **Autostart** — OS-native autostart registration (macOS AppleScript, Linux XDG, Windows registry)
- **Cross-platform** — Builds for Linux (deb, rpm, AppImage), macOS (.app), and Windows (MSI) including ARM64

## Prerequisites

- [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) (system dependencies)
- [Node.js](https://nodejs.org/) (for Tauri CLI and aw-webui build)
- [Rust](https://rustup.rs/) (stable toolchain)

## Quick start

```sh
# Clone with submodules (aw-webui)
git clone --recursive https://github.com/ActivityWatch/aw-tauri.git
cd aw-tauri

# Install Node dependencies
npm install

# Run in development mode
make dev

# Build release binary
make build
```

## Configuration

aw-tauri reads its config from a TOML file:

| Platform | Path |
|----------|------|
| Linux    | `~/.config/activitywatch/aw-tauri/config.toml` |
| macOS    | `~/Library/Application Support/activitywatch/aw-tauri/config.toml` |
| Windows  | `%APPDATA%\activitywatch\aw-tauri\config.toml` |

A default config is generated on first run. Example:

```toml
port = 5600
discovery_paths = [
  "/home/user/.local/bin",
  "/home/user/.local/share/activitywatch/aw-tauri/modules",
  "/home/user/aw-modules",
]

[autostart]
enabled = true
minimized = true
modules = [
  "aw-watcher-afk",
  "aw-watcher-window",
  { name = "aw-sync", args = "daemon" },
]

[module_args]
"aw-watcher-vscode" = "--poll-interval 30"
```

**Key settings:**
- `port` — Server port (default: 5600)
- `discovery_paths` — Additional directories to search for `aw-*` module binaries
- `autostart.enabled` — Register for OS autostart on login
- `autostart.minimized` — Start minimized to tray (if `false`, opens dashboard on launch)
- `autostart.modules` — Modules to start automatically. Each entry can be a string (`"aw-watcher-afk"`) or an object with args (`{ name = "aw-sync", args = "daemon" }`)
- `module_args` — Default args by module name, used when a module is launched from the tray menu or restarted after a crash. Lets you set args for a module *without* adding it to `autostart.modules` (e.g. `aw-watcher-vscode` above is launched manually from the tray, but still gets its args). If a module is listed in both places, the inline args on its `autostart.modules` entry take precedence.

**Note:** On Linux with Wayland, the default modules are `aw-awatcher` instead of `aw-watcher-afk` + `aw-watcher-window` (auto-detected via `XDG_SESSION_TYPE` / `WAYLAND_DISPLAY`).

## Logging

Logs are written to:

| Platform | Path |
|----------|------|
| Linux    | `~/.cache/activitywatch/aw-tauri/log/aw-tauri.log` |
| macOS    | `~/Library/Logs/activitywatch/aw-tauri/aw-tauri.log` |
| Windows  | `%LOCALAPPDATA%\activitywatch\Logs\aw-tauri\aw-tauri.log` |

Log rotation happens automatically at 32 MB, keeping the 5 most recent rotated logs.

**Environment variables:**
- `AW_DEBUG=1` — Enable debug-level logging
- `AW_TRACE=1` — Enable trace-level logging (very verbose)

## Architecture

```
┌─────────────────────────────────────────────┐
│                  aw-tauri                    │
│                                             │
│  ┌───────────┐  ┌─────────────────────────┐ │
│  │ Tray Icon │  │   WebView (aw-webui)    │ │
│  │  + Menu   │  │                         │ │
│  └─────┬─────┘  └─────────────────────────┘ │
│        │                                    │
│  ┌─────┴──────────────────────────────────┐ │
│  │          Module Manager                │ │
│  │  (discover, start, stop, restart)      │ │
│  └─────┬──────────────────────────────────┘ │
│        │                                    │
│  ┌─────┴──────────────────────────────────┐ │
│  │       aw-server-rust (embedded)        │ │
│  │         Rocket HTTP on :5600           │ │
│  └────────────────────────────────────────┘ │
└─────────────────────────────────────────────┘
        │
        │ spawns child processes
        ▼
┌──────────────┐  ┌──────────────────┐  ┌─────────┐
│ aw-watcher-  │  │ aw-watcher-      │  │aw-sync  │
│ afk          │  │ window           │  │ daemon  │
└──────────────┘  └──────────────────┘  └─────────┘
```

### Source layout

```
aw-tauri/
├── src-tauri/
│   ├── src/
│   │   ├── main.rs        # Entry point — calls lib::run()
│   │   ├── lib.rs         # Application setup: Tauri builder, server, tray, config
│   │   ├── manager.rs     # Module manager: discovery, lifecycle, crash recovery
│   │   ├── dirs.rs        # Platform-specific directory resolution
│   │   └── logging.rs     # Log setup with fern, rotation at 32 MB
│   ├── build.rs           # Build script — requires AW_WEBUI_DIR env var
│   ├── Cargo.toml         # Rust dependencies
│   ├── capabilities/      # Tauri v2 capability declarations
│   └── .cargo/config.toml # Sets AW_WEBUI_DIR to ../aw-webui/dist
├── aw-webui/              # Git submodule → ActivityWatch/aw-webui
├── Makefile               # Build orchestration
├── package.json           # Node.js config (Tauri CLI)
└── .pre-commit-config.yaml
```

### Key modules

**`lib.rs`** — The core of the application. Sets up:
- Config loading from TOML (with first-run defaults)
- aw-server-rust embedded via `build_rocket()` on an async Tauri runtime
- WebView window navigated to `http://localhost:{port}/`
- System tray with Open Dashboard / Modules submenu / Quit
- Single-instance enforcement via lockfile watching
- Autostart registration via `tauri-plugin-autostart`

**`manager.rs`** — Process manager for ActivityWatch modules:
- **Discovery**: Scans `PATH` + configured `discovery_paths` for executables matching `aw-*` prefix. On Unix, checks execute permission; on Windows, looks for `.exe` files. Excludes known non-module binaries (aw-tauri, aw-client, aw-server, etc.)
- **Lifecycle**: Starts modules as child processes, tracks PIDs, handles start/stop from tray menu
- **Crash recovery**: On non-zero exit, restarts with exponential backoff (2s, 4s, 8s). After 3 consecutive crashes, stops and notifies the user
- **Parent monitoring** (Unix): Creates a pipe so child processes are terminated if the parent dies
- **Job objects** (Windows): Assigns children to a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
- **aw-notify special handling**: Runs with `--output-only` flag, parses JSON notification output, and uses Tauri's notification API to display them natively

**`dirs.rs`** — Platform-aware directory resolution for config, data, logs, and runtime paths. Uses the `dirs` crate for standard locations. Includes Android stubs for future mobile support.

**`logging.rs`** — Logging via `fern` with colored console output. Log rotation at 32 MB with 5 rotated copies kept.

### Module discovery paths

aw-tauri searches for `aw-*` executables in these locations:

| Platform | Paths |
|----------|-------|
| Linux    | `~/bin`, `~/.local/bin`, `$XDG_DATA_HOME/activitywatch/aw-tauri/modules`, `~/aw-modules`, `$PATH` |
| macOS    | `~/aw-modules`, `/Applications/ActivityWatch.app/Contents/MacOS`, `/Applications/ActivityWatch.app/Contents/Resources`, `$PATH` |
| Windows  | `C:\Users\{user}\aw-modules`, `C:\Users\{user}\AppData\Local\Programs\ActivityWatch`, `%PATH%` |

Additional paths can be added via `discovery_paths` in the config.

## Development

### Prerequisites

Install [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform, plus Node.js and Rust.

### Commands

```sh
make dev        # Run in development mode (hot-reload for Rust)
make build      # Build release binary
make format     # Format Rust code (cargo fmt)
make check      # Run cargo check + clippy
make precommit  # Run format + check (same as pre-commit hooks)
make package    # Package built binary into dist/
```

### How the build works

1. `npm install` — Installs Tauri CLI
2. `make prebuild` — Initializes aw-webui submodule, builds aw-webui (`cd aw-webui && make build`), generates tray icons
3. `AW_WEBUI_DIR` env var (set in `src-tauri/.cargo/config.toml`) points the build to `aw-webui/dist/`
4. `npm run tauri build` — Compiles Rust code + bundles into platform-specific packages

### Pre-commit hooks

The repo uses [pre-commit](https://pre-commit.com/) with these hooks:
- `cargo fmt` — Rust formatting
- `cargo clippy -- -D warnings` — Lint with warnings as errors
- `cargo check` — Compilation check
- `end-of-file-fixer` — Ensure files end with newline
- `check-added-large-files` — Block files > 500 KB

### Running tests

```sh
cd src-tauri && cargo test
```

Currently tests cover directory resolution (`dirs.rs`). More test coverage is welcome.

## Troubleshooting

### Port already in use

If aw-tauri fails to start with "Port 5600 is already in use", another ActivityWatch server (or aw-tauri instance) is running. Stop it first, or change the `port` in your config.

### Modules not discovered

Check that your watcher binaries are in one of the [discovery paths](#module-discovery-paths) or add a custom path to `discovery_paths` in your config. On Unix, ensure the binaries have execute permission.

### Blank webview

If the dashboard shows a blank page, the aw-webui assets may not have been built. Run `cd aw-webui && make build` and restart.

## License

[MPL-2.0](LICENSE.txt)
