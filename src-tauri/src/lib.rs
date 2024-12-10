use aw_server::endpoints::build_rocket;
use directories::ProjectDirs;
use lazy_static::lazy_static;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::fs::{create_dir_all, read_to_string, remove_file, write, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::tray::TrayIconId;
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;

mod manager;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

static HANDLE: OnceLock<Mutex<AppHandle>> = OnceLock::new();
lazy_static! {
    static ref HANDLE_CONDVAR: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
}
static TRAY_ID: OnceLock<TrayIconId> = OnceLock::new();
static CONFIG: OnceLock<UserConfig> = OnceLock::new();
static FIRST_RUN: OnceLock<bool> = OnceLock::new();

fn init_app_handle(handle: AppHandle) {
    HANDLE.get_or_init(|| Mutex::new(handle));
    let (lock, cvar) = &*HANDLE_CONDVAR;
    let mut started = lock.lock().expect("failed to lock HANDLE_CONDVAR");
    *started = true;
    cvar.notify_all();
}

pub(crate) fn get_app_handle() -> &'static Mutex<AppHandle> {
    HANDLE.get().expect("HANDLE not initialized")
}

pub(crate) fn get_tray_id() -> &'static TrayIconId {
    TRAY_ID.get().expect("TRAY_ID not initialized")
}

pub(crate) fn is_first_run() -> &'static bool {
    FIRST_RUN.get().expect("FIRST_RUN not initialized")
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
pub struct UserConfig {
    pub autostart_modules: Vec<String>,
    pub autolaunch: bool,
    pub autostart_minimized: bool,
    pub port: u16,
}

impl Default for UserConfig {
    fn default() -> Self {
        UserConfig {
            autolaunch: true,
            autostart_minimized: true,
            autostart_modules: vec![
                "aw-watcher-afk".to_string(),
                "aw-watcher-window".to_string(),
                "aw-awatcher".to_string(),
            ],
            port: 5699, // TODO: update before going stable
        }
    }
}

fn get_config_path() -> PathBuf {
    let project_dirs =
        ProjectDirs::from("net", "ActivityWatch", "Aw-Tauri").expect("Failed to get project dirs");
    let config_path = project_dirs.config_dir().join("config.toml");
    config_path
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
    tauri::Builder::default()
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
            println!("Another instance is running, quitting!");
        }))
        .setup(|app| {
            {
                init_app_handle(app.handle().clone());
                let user_config = get_config();
                // Get the autostart manager
                let autostart_manager = app.autolaunch();

                match user_config.autolaunch {
                    true => {
                        autostart_manager
                            .enable()
                            .expect("Unable to enable autostart");
                    }
                    false => {
                        autostart_manager
                            .disable()
                            .expect("Unable to disable autosart");
                    }
                }

                // Check enable state
                println!(
                    "registered for autostart? {}",
                    autostart_manager
                        .is_enabled()
                        .expect("failed to get autostart state")
                );

                let testing = true;
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
                        println!("Using webui path: {}", path_str);
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

                tauri::async_runtime::spawn(build_rocket(server_state, aw_config).launch());

                let manager_state = manager::start_manager();

                let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
                    .expect("failed to create open menu item");
                let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
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
                    .menu_on_left_click(true)
                    .build(app)
                    .expect("failed to create tray");

                TRAY_ID
                    .set(tray.id().clone())
                    .expect("failed to set TRAY_ID");
                app.on_menu_event(move |app, event| {
                    if event.id() == open.id() {
                        println!("system tray received a open click");
                        let windows = app.webview_windows();
                        let window = windows.get("main").expect("main window not found");
                        window.show().unwrap();
                    } else if event.id() == quit.id() {
                        println!("quit clicked!");
                        let state = manager_state.lock().unwrap();
                        state.stop_modules();
                        app.exit(0);
                    } else {
                        // Modules menu clicks
                        let mut state = manager_state.lock().unwrap();
                        state.handle_system_click(&event.id().0);
                    }
                });
                if user_config.autolaunch && user_config.autostart_minimized {
                    if let Some(window) = app.webview_windows().get("main") {
                        window.hide().unwrap();
                    }
                }
            }

            let first_run = is_first_run();
            if *first_run {
                thread::spawn(|| {
                    // TODO: debug and remove the sleep
                    thread::sleep(Duration::from_secs(1));
                    let app = &*get_app_handle().lock().expect("failed to get app handle");
                    app.notification()
                        .builder()
                        .title("Aw-Tauri")
                        .body("Aw-Tauri is running in the background")
                        .show()
                        .unwrap();
                });
            }
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
