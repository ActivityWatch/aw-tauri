aw-tauri
========

Experimenting with implementing ActivityWatch using [Tauri](https://tauri.app/).

Holds great promise as a much simpler way to build a cross-platform version of ActivityWatch.

Features:

 - Tray icon
 - Module manager for watchers
 - WebView serving the web UI
 - Uses aw-server-rust by default
 - Replaces aw-qt
 - Builds like a dream, minimal custom build & release config

Benefits of Tauri:

 - Builds cross-platform nicely (see [their docs](https://tauri.app/v1/guides/building/cross-platform))
   - Generates deb and AppImage with a simple `npx tauri build`
   - Uses Gtk on Linux, and [tao](https://github.com/tauri-apps/tao) on Windows and macOS
   - No more messy PyInstaller for the main entrypoint (aw-qt)
   - Good [docs for code-signing](https://tauri.app/v1/guides/distribution/sign-windows) on all platforms
   - Includes an [updater](https://tauri.app/v1/guides/distribution/updater) for `MSI`, `.AppImage`, `.app` bundle.
 - Contains a webview with an easy interface to Rust code
 - [Trayicon support](https://tauri.app/v1/guides/features/system-tray/)
 - Mobile support is [WIP](https://tauri.app/blog/2022/12/09/tauri-mobile-alpha/), and will support iOS.

# Prerequisites

 - Node.js
 - Rust
 - Tauri CLI (`npm install -g @tauri-apps/cli`)
 - Tauri dependencies (see [their docs](https://v2.tauri.app/start/prerequisites/))

# Usage

To run:

```sh
npm install
npm run tauri dev
```

# Repo stucture

 - The frontend is in the root folder (NOTE: not yet the actual aw-webui code)
 - All rust code is in `src-tauri/` (will likely be moved)

# Roadmap

Primary goal is feature-parity with aw-qt.
Secondary goal is to add extras supported by Tauri (updater, autostart).

 - [x] Run aw-server-rust as part of main executable
 - [x] Run ActivityWatch web app within WebView (wry)
 - [x] Get basic module manager working
     - [x] Start watchers
 - [x] Tray icon
     - [x] Basic version (open, exit)
     - [x] Menu for module manager
     - [x] Responsive running/stopped state for watchers (no "update" button)
     - [x] Start/stop via modules menu
     - [x] Detect bundled & system modules
 - [ ] Polish
     - [ ] Remove placeholder Vue app
         - Or replace with new UI for module management? (a bit redundant)
     - [x] Build aw-webui as part of build process
     - [ ] Error dialog when module crashes, restart logic
         - https://tauri.app/plugin/dialog/
     - [x] https://tauri.app/plugin/autostart/
     - [x] https://tauri.app/plugin/single-instance/

---

This project was initialized with:

```sh
sh <(curl https://create.tauri.app/sh)
```

<details>
<summary>Click to expand original README</summary>

# Tauri + Vue 3 + TypeScript

This template should help get you started developing with Vue 3 and TypeScript in Vite. The template uses Vue 3 `<script setup>` SFCs, check out the [script setup docs](https://v3.vuejs.org/api/sfc-script-setup.html#sfc-script-setup) to learn more.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Volar](https://marketplace.visualstudio.com/items?itemName=Vue.volar) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Type Support For `.vue` Imports in TS

Since TypeScript cannot handle type information for `.vue` imports, they are shimmed to be a generic Vue component type by default. In most cases this is fine if you don't really care about component prop types outside of templates. However, if you wish to get actual prop types in `.vue` imports (for example to get props validation when using manual `h(...)` calls), you can enable Volar's Take Over mode by following these steps:

1. Run `Extensions: Show Built-in Extensions` from VS Code's command palette, look for `TypeScript and JavaScript Language Features`, then right click and select `Disable (Workspace)`. By default, Take Over mode will enable itself if the default TypeScript extension is disabled.
2. Reload the VS Code window by running `Developer: Reload Window` from the command palette.

You can learn more about Take Over mode [here](https://github.com/johnsoncodehk/volar/discussions/471).

</details>
