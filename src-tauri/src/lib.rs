use aw_server::endpoints::build_rocket;
use lazy_static::lazy_static;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{create_dir_all, read_to_string, remove_file, write, OpenOptions};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

mod dirs;
mod logging;
mod manager;

use log::{info, trace, warn};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{TrayIconBuilder, TrayIconId},
    AppHandle, Manager,
};

pub struct AppHandleWrapper(Mutex<AppHandle>);

impl Drop for AppHandleWrapper {
    fn drop(&mut self) {
        let (_lock, cvar) = &*HANDLE_CONDVAR;
        cvar.notify_all();
    }
}

static HANDLE: OnceLock<AppHandleWrapper> = OnceLock::new();
lazy_static! {
    static ref HANDLE_CONDVAR: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
}
#[derive(Debug)]
pub struct TrayIdWrapper(TrayIconId);

impl Drop for TrayIdWrapper {
    fn drop(&mut self) {
        let (_lock, cvar) = &*TRAY_CONDVAR;
        cvar.notify_all();
    }
}

static TRAY_ID: OnceLock<TrayIdWrapper> = OnceLock::new();
lazy_static! {
    static ref TRAY_CONDVAR: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
}
static CONFIG: OnceLock<UserConfig> = OnceLock::new();
static FIRST_RUN: OnceLock<bool> = OnceLock::new();

fn init_app_handle(handle: AppHandle) {
    HANDLE.get_or_init(|| AppHandleWrapper(Mutex::new(handle)));
    let (lock, cvar) = &*HANDLE_CONDVAR;
    let mut started = lock.lock().expect("Failed to lock HANDLE_CONDVAR");
    *started = true;
    cvar.notify_all();
}

pub(crate) fn get_app_handle() -> &'static Mutex<AppHandle> {
    &HANDLE.get().expect("HANDLE not initialized").0
}

fn init_tray_id(id: TrayIconId) {
    TRAY_ID
        .set(TrayIdWrapper(id))
        .expect("failed to set TRAY_ID");
    let (lock, cvar) = &*TRAY_CONDVAR;
    let mut initialized = lock.lock().expect("Failed to lock TRAY_CONDVAR");
    *initialized = true;
    cvar.notify_all();
}

pub(crate) fn get_tray_id() -> &'static TrayIconId {
    let (lock, cvar) = &*TRAY_CONDVAR;
    let mut initialized = lock.lock().expect("Failed to lock TRAY_CONDVAR");
    while !*initialized {
        initialized = cvar.wait(initialized).expect("Failed to wait for TRAY_ID");
    }
    &TRAY_ID.get().expect("TRAY_ID not initialized").0
}

fn write_formatted_config(config: &UserConfig, path: &Path) -> Result<(), std::io::Error> {
    // Helper function to write the config prettier
    let mut output = String::new();

    output.push_str(&format!("port = {}\n", config.port));

    output.push_str("discovery_paths = [");
    if !config.discovery_paths.is_empty() {
        output.push('\n');
        for path in &config.discovery_paths {
            output.push_str(&format!("  \"{}\",\n", path.to_str().unwrap_or_default()));
        }
        output.push(']');
    } else {
        output.push_str("]\n");
    }
    output.push_str("\n\n");

    // Add autostart section
    output.push_str("[autostart]\n");
    output.push_str(&format!("enabled = {}\n", config.autostart.enabled));
    output.push_str(&format!("minimized = {}\n", config.autostart.minimized));

    // Format modules with one per line
    output.push_str("modules = [\n");
    for module in &config.autostart.modules {
        match module {
            ModuleEntry::Simple(name) => {
                output.push_str(&format!("  \"{}\",\n", name));
            }
            ModuleEntry::Full { name, args } => {
                output.push_str(&format!(
                    "  {{ name = \"{}\", args = \"{}\" }},\n",
                    name, args
                ));
            }
        }
    }

    if !config.autostart.modules.is_empty() {
        output.truncate(output.len() - 2); // Remove last comma and newline
        output.push('\n'); // Add back just the newline
    }
    output.push_str("]\n");

    write(path, output)
}

pub fn is_port_available(port: u16) -> std::io::Result<bool> {
    let addr = format!("127.0.0.1:{}", port)
        .parse::<SocketAddr>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    match TcpListener::bind(addr) {
        Ok(_) => Ok(true), // Port is available
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                Ok(false) // Port is in use
            } else {
                Err(e) // Other error occurred
            }
        }
    }
}

pub(crate) fn is_first_run() -> &'static bool {
    FIRST_RUN.get().expect("FIRST_RUN not initialized")
}

pub fn handle_first_run() {
    let first_run = is_first_run();
    if *first_run {
        thread::spawn(|| {
            let app = &*get_app_handle().lock().expect("Failed to get app handle");
            app.notification()
                .builder()
                .title("Aw-Tauri")
                .body("Welcome to Aw-Tauri! Click on the tray icon to launch the dashboard")
                .show()
                .expect("Failed to show first run notification");
            if let Some(window) = app.webview_windows().get("main") {
                window.show().expect("Failed to show main window");
            }
        });
    }
}

pub fn listen_for_lockfile() {
    thread::spawn(|| {
        let runtime_path = get_runtime_path();
        let watcher = SpecificFileWatcher::new(&runtime_path, "single_instance.lock")
            .expect("Failed to create file watcher");
        loop {
            if watcher.wait_for_file().is_ok() {
                remove_file(get_runtime_path().join("single_instance.lock"))
                    .expect("Failed to remove lock file");
                let app = &*get_app_handle().lock().expect("Failed to get app handle");
                if let Some(window) = app.webview_windows().get("main") {
                    window.show().expect("Failed to show main window");
                }
            }
        }
    });
}

pub struct SpecificFileWatcher {
    #[allow(dead_code)]
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<Result<Event, notify::Error>>,
    target_file: PathBuf,
}

impl SpecificFileWatcher {
    pub fn new<P: AsRef<Path>>(dir_path: P, filename: &str) -> Result<Self, notify::Error> {
        let (tx, rx) = mpsc::channel();

        let target_file = dir_path.as_ref().join(filename);

        // Configure the watcher with minimal overhead
        let config = Config::default().with_poll_interval(Duration::from_secs(1));

        // Create a watcher
        let mut watcher = RecommendedWatcher::new(tx, config)?;

        watcher.watch(dir_path.as_ref(), RecursiveMode::NonRecursive)?;

        Ok(Self {
            watcher,
            rx,
            target_file,
        })
    }

    pub fn wait_for_file(&self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // Check for events
            if let Ok(result) = self.rx.try_recv() {
                match result {
                    Ok(event) => match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            if event.paths.iter().any(|p| p == &self.target_file) {
                                return Ok(());
                            }
                        }
                        _ => {}
                    },
                    Err(e) => warn!("Watch error: {}", e),
                }
            }

            // Avoid busy waiting
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

// Module representation that can be either a string or an object with name/args
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModuleEntry {
    Simple(String),
    Full {
        name: String,
        #[serde(default = "String::new")]
        args: String,
    },
}

impl ModuleEntry {
    pub fn name(&self) -> &str {
        match self {
            ModuleEntry::Simple(name) => name,
            ModuleEntry::Full { name, .. } => name,
        }
    }

    pub fn args(&self) -> &str {
        match self {
            ModuleEntry::Simple(_) => "",
            ModuleEntry::Full { args, .. } => args,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AutostartConfig {
    pub enabled: bool,
    pub minimized: bool,
    pub modules: Vec<ModuleEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserConfig {
    pub port: u16,
    pub discovery_paths: Vec<PathBuf>,
    pub autostart: AutostartConfig,
}

impl Default for UserConfig {
    fn default() -> Self {
        let discovery_paths = dirs::get_discovery_paths();

        // Build default modules list based on platform and display server
        let mut modules = Vec::new();

        if cfg!(target_os = "linux") {
            // Check for Wayland using multiple environment variables
            let is_wayland = env::var("XDG_SESSION_TYPE")
                .map(|s| s == "wayland")
                .unwrap_or(false)
                || env::var("WAYLAND_DISPLAY").is_ok();

            if is_wayland {
                // On Linux with Wayland, use aw-awatcher instead of separate watchers
                modules.push(ModuleEntry::Simple("aw-awatcher".to_string()));
            } else {
                // On Linux with X11 or other display servers, use traditional watchers
                modules.push(ModuleEntry::Simple("aw-watcher-afk".to_string()));
                modules.push(ModuleEntry::Simple("aw-watcher-window".to_string()));
            }
        } else {
            // On non-Linux platforms, use traditional watchers
            modules.push(ModuleEntry::Simple("aw-watcher-afk".to_string()));
            modules.push(ModuleEntry::Simple("aw-watcher-window".to_string()));
        }

        modules.push(ModuleEntry::Full {
            name: "aw-sync".to_string(),
            args: "daemon".to_string(),
        });

        UserConfig {
            port: 5600,
            discovery_paths,
            autostart: AutostartConfig {
                enabled: true,
                minimized: true,
                modules,
            },
        }
    }
}

fn get_config_path() -> PathBuf {
    dirs::get_config_path()
}

fn get_runtime_path() -> PathBuf {
    dirs::get_runtime_dir()
}

pub(crate) fn get_config() -> &'static UserConfig {
    CONFIG.get_or_init(|| {
        let config_path = get_config_path();
        if config_path.exists() {
            FIRST_RUN.set(false).expect("Failed to set FIRST_RUN");
            let config_str = read_to_string(&config_path).expect("Failed to read config file");

            // Try to parse the config file
            match toml::from_str::<UserConfig>(&config_str) {
                Ok(config) => config,
                Err(e) => {
                    warn!("Failed to parse config file: {}. Using default config.", e);

                    let app = &*get_app_handle().lock().expect("Failed to get app handle");
                    app.dialog()
                        .message("Malformed config file. Using default config.")
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .show(|_| {});

                    UserConfig::default()
                }
            }
        } else {
            FIRST_RUN.set(true).expect("failed to set FIRST_RUN");

            let config = UserConfig::default();
            create_dir_all(config_path.parent().unwrap()).expect("Failed to create config dir");
            write_formatted_config(&config, &config_path).expect("Failed to write config file");
            config
        }
    })
}

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Rotate log if needed (before initializing logging)
    if let Err(e) = logging::rotate_log_if_needed() {
        eprintln!("Failed to rotate log: {}", e);
    }

    // Initialize logging
    if let Err(e) = logging::setup_logging() {
        // Can't use log here since logging isn't initialized yet
        eprintln!("Failed to initialize logging: {}", e);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::AppleScript,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            let lock_path = get_runtime_path().join("single_instance.lock");
            if !lock_path.parent().unwrap().exists() {
                create_dir_all(lock_path.parent().unwrap()).expect("Failed to create runtime dir");
            }
            let _lock_file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(lock_path)
                .expect("Failed to open lock file");
            info!("Another instance is running, quitting!");
        }))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            {
                //TODO: Some of this setup could run concurrently. Could slash a few 100ms in startup?
                init_app_handle(app.handle().clone());
                let user_config = get_config();
                // Get the autostart manager
                let autostart_manager = app.autolaunch();

                match user_config.autostart.enabled {
                    true => {
                        if !autostart_manager
                            .is_enabled()
                            .expect("Failed to get autostart state")
                        {
                            autostart_manager
                                .enable()
                                .expect("Unable to enable autostart");
                            info!("Registered for autostart: true");
                        }
                    }
                    false => {
                        //checks for state before disabling no need to check twice
                        autostart_manager
                            .disable()
                            .expect("Unable to disable autosart");
                        info!("Registered for autostart: false");
                    }
                }

                let testing = false;
                let legacy_import = false;

                let mut aw_config = aw_server::config::create_config(testing);
                aw_config.port = user_config.port;
                let db_path = aw_server::dirs::db_path(testing)
                    .expect("Failed to get db path")
                    .to_str()
                    .unwrap()
                    .to_string();
                let device_id = aw_server::device_id::get_device_id();

                let webui_var = std::env::var("AW_WEBUI_DIR");

                let asset_path_opt = if let Ok(path_str) = &webui_var {
                    let asset_path = PathBuf::from(&path_str);
                    if asset_path.exists() {
                        info!("Using webui path: {}", path_str);
                        Some(asset_path)
                    } else {
                        panic!("Path set via env var AW_WEBUI_DIR does not exist");
                    }
                } else {
                    info!("Using bundled assets");
                    None
                };

                let server_state = aw_server::endpoints::ServerState {
                    // Even if legacy_import is set to true it is disabled on Android so
                    // it will not happen there
                    datastore: Mutex::new(aw_datastore::Datastore::new(db_path, legacy_import)),
                    asset_resolver: aw_server::endpoints::AssetResolver::new(asset_path_opt),
                    device_id,
                };
                if !is_port_available(user_config.port).expect("Failed to check port availability")
                {
                    app.dialog()
                        .message(format!("Port {} is already in use", user_config.port))
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .show(|_| {});
                    panic!("Port {} is already in use", user_config.port);
                }
                tauri::async_runtime::spawn(build_rocket(server_state, aw_config).launch());
                let url = format!("http://localhost:{}/", user_config.port)
                    .parse()
                    .expect("Failed to parse localhost url");
                let mut main_window = app
                    .get_webview_window("main")
                    .expect("Failed to show main window");

                main_window
                    .navigate(url)
                    .expect("Error navigating main window");
                let manager_state = manager::start_manager();

                let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)
                    .expect("Failed to create open menu item");
                let quit = MenuItem::with_id(app, "quit", "Quit ActivityWatch", true, None::<&str>)
                    .expect("Failed to create quit menu item");

                let menu =
                    Menu::with_items(app, &[&open, &quit]).expect("Failed to create tray menu");

                #[cfg(not(target_os = "windows"))]
                let tray_builder = TrayIconBuilder::new()
                    .icon(
                        app.default_window_icon()
                            .expect("Failed to get window icon")
                            .clone(),
                    )
                    .menu(&menu)
                    .show_menu_on_left_click(true);

                #[cfg(target_os = "windows")]
                let tray_builder = TrayIconBuilder::new()
                    .icon(
                        app.default_window_icon()
                            .expect("Failed to get window icon")
                            .clone(),
                    )
                    .menu(&menu)
                    .show_menu_on_left_click(true)
                    .tooltip("ActivityWatch");
                let tray = tray_builder.build(app).expect("Failed to create tray");

                init_tray_id(tray.id().clone());
                app.on_menu_event(move |app, event| {
                    if event.id().0 == "open" {
                        trace!("system tray received a open click");
                        let windows = app.webview_windows();
                        let window = windows.get("main").expect("Main window not found");
                        window.show().expect("Failed to show window");
                        window.set_focus().expect("Failed to focus window");
                    } else if event.id().0 == "quit" {
                        trace!("quit clicked!");
                        let mut state = manager_state
                            .lock()
                            .expect("Failed to acquire manager_state lock");
                        state.stop_modules();
                        app.exit(0);
                    } else if event.id().0 == "config_folder" {
                        let config_path = get_config_path();
                        let config_dir = config_path.parent().unwrap_or(&config_path);
                        app.opener()
                            .reveal_item_in_dir(config_dir)
                            .expect("Failed to open config folder");
                    } else if event.id().0 == "log_folder" {
                        let log_path = logging::get_log_path();
                        let log_dir = log_path.parent().unwrap_or(&log_path);
                        app.opener()
                            .reveal_item_in_dir(log_dir)
                            .expect("Failed to open log folder");
                    } else {
                        // Modules menu clicks
                        let mut state = manager_state
                            .lock()
                            .expect("Failed to acquire manager_state lock");
                        state.handle_system_click(&event.id().0);
                    }
                });
                if user_config.autostart.enabled && !user_config.autostart.minimized {
                    if let Some(window) = app.webview_windows().get("main") {
                        window.show().expect("Failed to show main window");
                    }
                }
            }

            handle_first_run();
            listen_for_lockfile();
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = &event {
                api.prevent_close();
                window.hide().expect("Failed to hide main window");
            };
        })
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
