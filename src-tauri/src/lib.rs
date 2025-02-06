use aw_server::endpoints::build_rocket;
#[cfg(not(target_os = "linux"))]
use directories::ProjectDirs;
use directories::UserDirs;
use lazy_static::lazy_static;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
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

mod logging;
mod manager;
mod modules_dl;

use log::info;
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
    let mut started = lock.lock().expect("failed to lock HANDLE_CONDVAR");
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
    let mut initialized = lock.lock().expect("failed to lock TRAY_CONDVAR");
    *initialized = true;
    cvar.notify_all();
}

pub(crate) fn get_tray_id() -> &'static TrayIconId {
    let (lock, cvar) = &*TRAY_CONDVAR;
    let mut initialized = lock.lock().expect("failed to lock TRAY_CONDVAR");
    while !*initialized {
        initialized = cvar.wait(initialized).expect("failed to wait for TRAY_ID");
    }
    &TRAY_ID.get().expect("TRAY_ID not initialized").0
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
            let app = &*get_app_handle().lock().expect("failed to get app handle");
            app.notification()
                .builder()
                .title("Aw-Tauri")
                .body("Aw-Tauri is running in the background")
                .show()
                .unwrap();
        });
    }
}

pub fn listen_for_lockfile() {
    thread::spawn(|| {
        let config_path = get_config_path();
        let watcher =
            SpecificFileWatcher::new(config_path.parent().unwrap(), "single_instance.lock")
                .expect("Failed to create file watcher");
        loop {
            if watcher.wait_for_file().is_ok() {
                remove_file(config_path.parent().unwrap().join("single_instance.lock"))
                    .expect("Failed to remove lock file");
                let app = &*get_app_handle().lock().expect("failed to get app handle");
                if let Some(window) = app.webview_windows().get("main") {
                    window.show().unwrap();
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
                    Err(e) => eprintln!("Watch error: {}", e),
                }
            }

            // Avoid busy waiting
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModuleConfig {
    pub name: String,
    #[serde(default = "String::new")]
    pub args: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Defaults {
    pub autostart: bool,
    pub autostart_minimized: bool,
    pub port: u16,
    pub discovery_path: PathBuf,
}

impl Default for Defaults {
    fn default() -> Self {
        let discovery_path = if cfg!(target_os = "linux") {
            UserDirs::new()
                .map(|dirs| dirs.home_dir().join("aw-modules"))
                .unwrap_or_default()
        } else if cfg!(windows) {
            let username = std::env::var("USERNAME").unwrap_or_default();
            PathBuf::from(format!(r"C:\Users\{}\aw-modules", username))
        } else if cfg!(target_os = "macos") {
            PathBuf::from("/Applications/ActivityWatch.app/Contents/MacOS")
        } else {
            PathBuf::new()
        };

        Defaults {
            autostart: true,
            autostart_minimized: true,
            port: 5699, // TODO: update before going stable
            discovery_path,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub autostart_modules: Vec<ModuleConfig>,
}

impl Default for UserConfig {
    fn default() -> Self {
        UserConfig {
            defaults: Defaults::default(),
            autostart_modules: vec![
                ModuleConfig {
                    name: "aw-watcher-afk".to_string(),
                    args: String::new(),
                },
                ModuleConfig {
                    name: "aw-watcher-window".to_string(),
                    args: String::new(),
                },
                ModuleConfig {
                    name: "aw-awatcher".to_string(),
                    args: String::new(),
                },
            ],
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn get_config_path() -> PathBuf {
    let project_dirs =
        ProjectDirs::from("net", "ActivityWatch", "Aw-Tauri").expect("Failed to get project dirs");
    project_dirs.config_dir().join("config.toml")
}
#[cfg(target_os = "linux")]
fn get_config_path() -> PathBuf {
    let userdirs = UserDirs::new().expect("Failed to get user dirs");
    let home = userdirs.home_dir();
    let config_dir = home.join(".config/activitywatch/aw-tauri");
    config_dir.join("config.toml")
}
pub(crate) fn get_config() -> &'static UserConfig {
    CONFIG.get_or_init(|| {
        let config_path = get_config_path();
        if config_path.exists() {
            FIRST_RUN.set(false).expect("failed to set FIRST_RUN");
            let config_str = read_to_string(config_path).expect("Failed to read config file");
            toml::from_str(&config_str).expect("Failed to parse config file")
        } else {
            FIRST_RUN.set(true).expect("failed to set FIRST_RUN");

            let config = UserConfig::default();
            let config_str = toml::to_string(&config).expect("Failed to serialize config");
            create_dir_all(config_path.parent().unwrap()).expect("Failed to create config dir");
            write(config_path, config_str).expect("Failed to write config file");
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
    // Initialize logging
    if let Err(e) = logging::setup_logging() {
        eprintln!("Failed to initialize logging: {}", e);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            let lock_path = get_config_path()
                .parent()
                .unwrap()
                .join("single_instance.lock");
            if !lock_path.parent().unwrap().exists() {
                create_dir_all(lock_path.parent().unwrap()).expect("Failed to create lock dir");
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

                match user_config.defaults.autostart {
                    true => {
                        if !autostart_manager
                            .is_enabled()
                            .expect("failed to get autostart state")
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

                let testing = true;
                let legacy_import = false;

                let mut aw_config = aw_server::config::create_config(testing);
                aw_config.port = user_config.defaults.port;
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
                    println!("Using bundled assets");
                    None
                };

                let server_state = aw_server::endpoints::ServerState {
                    // Even if legacy_import is set to true it is disabled on Android so
                    // it will not happen there
                    datastore: Mutex::new(aw_datastore::Datastore::new(db_path, legacy_import)),
                    asset_resolver: aw_server::endpoints::AssetResolver::new(asset_path_opt),
                    device_id,
                };
                if !is_port_available(user_config.defaults.port)
                    .expect("Failed to check port availability")
                {
                    app.dialog()
                        .message(format!(
                            "Port {} is already in use",
                            user_config.defaults.port
                        ))
                        .kind(MessageDialogKind::Error)
                        .title("Aw-Tauri")
                        .show(|_| {});
                    panic!("Port {} is already in use", user_config.defaults.port);
                }
                tauri::async_runtime::spawn(build_rocket(server_state, aw_config).launch());
                let url = format!("http://localhost:{}/", user_config.defaults.port)
                    .parse()
                    .unwrap();
                let mut main_window = app.get_webview_window("main").unwrap();

                main_window
                    .navigate(url)
                    .expect("error navigating main window");
                let manager_state = manager::start_manager();

                let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)
                    .expect("failed to create open menu item");
                let quit = MenuItem::with_id(app, "quit", "Quit ActivityWatch", true, None::<&str>)
                    .expect("failed to create quit menu item");

                let menu =
                    Menu::with_items(app, &[&open, &quit]).expect("failed to create tray menu");

                let tray = TrayIconBuilder::new()
                    .icon(
                        app.default_window_icon()
                            .expect("failed to get window icon")
                            .clone(),
                    )
                    .menu(&menu)
                    .show_menu_on_left_click(true)
                    .build(app)
                    .expect("failed to create tray");

                init_tray_id(tray.id().clone());
                app.on_menu_event(move |app, event| {
                    if event.id().0 == "open" {
                        println!("system tray received a open click");
                        let windows = app.webview_windows();
                        let window = windows.get("main").expect("main window not found");
                        window.show().unwrap();
                    } else if event.id().0 == "quit" {
                        println!("quit clicked!");
                        let state = manager_state.lock().unwrap();
                        state.stop_modules();
                        app.exit(0);
                    } else if event.id().0 == "config_folder" {
                        let config_path = get_config_path();
                        let config_dir = config_path.parent().unwrap_or(&config_path);
                        app.opener()
                            .reveal_item_in_dir(config_dir)
                            .expect("Failed to open config folder");
                    } else if event.id().0 == "log_folder" {
                        let log_path = logging::get_log_filepath();
                        let log_dir = log_path.parent().unwrap_or(&log_path);
                        app.opener()
                            .reveal_item_in_dir(log_dir)
                            .expect("Failed to open log folder");
                    } else {
                        // Modules menu clicks
                        let mut state = manager_state.lock().unwrap();
                        state.handle_system_click(&event.id().0);
                    }
                });
                if user_config.defaults.autostart
                    && user_config.defaults.autostart_minimized
                    && !*is_first_run()
                {
                    if let Some(window) = app.webview_windows().get("main") {
                        window.hide().unwrap();
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
                window.hide().unwrap();
            };
        })
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
