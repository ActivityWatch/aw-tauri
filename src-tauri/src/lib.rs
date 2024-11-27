use aw_server::endpoints::build_rocket;
use lazy_static::lazy_static;
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

fn init_app_handle(handle: AppHandle) {
    HANDLE.get_or_init(|| Mutex::new(handle));
    let (lock, cvar) = &*HANDLE_CONDVAR;
    let mut started = lock.lock().unwrap();
    *started = true;
    cvar.notify_all();
}

pub(crate) fn get_app_handle() -> &'static Mutex<AppHandle> {
    HANDLE.get().unwrap()
}

pub(crate) fn get_tray_id() -> &'static TrayIconId {
    TRAY_ID.get().unwrap()
}
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            println!("Another instance is running, quitting!");
        }))
        .setup(|app| {
            {
                // Get the autostart manager
                let autostart_manager = app.autolaunch();
                // Enable autostart
                let _ = autostart_manager.enable();
                // Check enable state
                println!(
                    "registered for autostart? {}",
                    autostart_manager.is_enabled().unwrap()
                );

                let testing = true;
                let legacy_import = false;

                let mut config = aw_server::config::create_config(testing);
                config.port = 5699;
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

                tauri::async_runtime::spawn(build_rocket(server_state, config).launch());

                let manager_state = manager::start_manager();

                let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
                    .expect("failed to create open menu item");
                let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
                    .expect("failed to create quit menu item");

                let menu =
                    Menu::with_items(app, &[&open, &quit]).expect("failed to create tray menu");

                let tray = TrayIconBuilder::new()
                    .icon(app.default_window_icon().unwrap().clone())
                    .menu(&menu)
                    .menu_on_left_click(true)
                    .build(app)
                    .expect("failed to create tray");

                //NOTE: init_app_handle must be called after TRAY_ID.set
                TRAY_ID.set(tray.id().clone()).unwrap();
                init_app_handle(app.handle().clone());

                app.on_menu_event(move |app, event| {
                    if event.id() == open.id() {
                        println!("system tray received a open click");
                        let windows = app.webview_windows();
                        let window = windows.get("main").unwrap();
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
