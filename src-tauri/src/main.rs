// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::Manager;
use tauri::SystemTray;
use tauri::{AppHandle, SystemTrayEvent};
use tauri::{CustomMenuItem, SystemTrayMenu, SystemTrayMenuItem};

use aw_datastore::Datastore;
use aw_server::endpoints::build_rocket;

// Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

fn main() {
    let testing = true;

    let mut config = aw_server::config::create_config(testing);
    config.port = 5699;
    let db_path = aw_server::dirs::db_path(testing)
        .expect("Failed to get db path")
        .to_str()
        .unwrap()
        .to_string();

    let device_id = aw_server::device_id::get_device_id();
    let tray = create_tray();
    tauri::Builder::default()
        .setup(|app| {
            let mut asset_path: Option<PathBuf> = None;
            for path in &[
                aw_server::dirs::get_asset_path(),
                PathBuf::from("../../aw-webui/dist".to_string()),
            ] {
                if path.exists() {
                    asset_path = Some(path.to_path_buf());
                    break;
                }
            }

            if asset_path.is_none() {
                panic!("Asset path does not exist");
            }

            let legacy_import = false;
            let server_state = aw_server::endpoints::ServerState {
                // Even if legacy_import is set to true it is disabled on Android so
                // it will not happen there
                datastore: Mutex::new(aw_datastore::Datastore::new(db_path, legacy_import)),
                asset_path: asset_path.unwrap(),
                device_id,
            };

            tauri::async_runtime::spawn(build_rocket(server_state, config).launch());
            Ok(())
        })
        .system_tray(tray)
        .on_system_tray_event(on_tray_event)
        .on_window_event(|event| match event.event() {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                event.window().hide().unwrap();
                api.prevent_close();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![greet])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app_handle, event| match event {
            tauri::RunEvent::ExitRequested { api, .. } => {
                api.prevent_exit();
            }
            _ => {}
        });
}

fn create_tray() -> SystemTray {
    // here `"quit".to_string()` defines the menu item id, and the second parameter is the menu item label.
    let quit = CustomMenuItem::new("quit".to_string(), "Quit");
    let open = CustomMenuItem::new("open".to_string(), "Open");
    let tray_menu = SystemTrayMenu::new()
        .add_item(quit)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(open);

    SystemTray::new().with_menu(tray_menu)
}

fn on_tray_event(app: &AppHandle, event: SystemTrayEvent) {
    match event {
        SystemTrayEvent::DoubleClick {
            position: _,
            size: _,
            ..
        } => {
            println!("system tray received a double click");
            let window = app.get_window("main").unwrap();
            window.show().unwrap();
        }
        SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
            "quit" => {
                println!("system tray received a quit click");
                app.exit(0);
            }
            "open" => {
                println!("system tray received a open click");
                let window = app.get_window("main").unwrap();
                window.show().unwrap();
            }
            _ => {}
        },
        _ => {}
    }
}
