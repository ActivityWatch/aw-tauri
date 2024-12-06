use aw_server::endpoints::build_rocket;
use directories::ProjectDirs;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use tauri::tray::TrayIconId;
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt;

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
static CONFIG: OnceLock<Config> = OnceLock::new();

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

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub autostart_modules: Vec<String>,
    pub autolaunch: bool,
    pub autostart_minimized: bool,
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            autolaunch: true,
            autostart_minimized: true, // TODO: implement this
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
    let config_dir = project_dirs.config_dir();
    let config_path = config_dir.join("config.toml");
    config_path
}

pub(crate) fn get_config() -> &'static Config {
    CONFIG.get_or_init(|| {
        let config_path = get_config_path();
        if config_path.exists() {
            let config_str =
                std::fs::read_to_string(config_path).expect("Failed to read config file");
            toml::from_str(&config_str).expect("Failed to parse config file")
        } else {
            let config = Config::default();
            let config_str = toml::to_string(&config).expect("Failed to serialize config");
            std::fs::write(config_path, config_str).expect("Failed to write config file");
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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            println!("Another instance is running, quitting!");
        }))
        .setup(|app| {
            {
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

                //NOTE: init_app_handle must be called after TRAY_ID.set
                TRAY_ID
                    .set(tray.id().clone())
                    .expect("failed to set TRAY_ID");
                init_app_handle(app.handle().clone());

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
            }

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
