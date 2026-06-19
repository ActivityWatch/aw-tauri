// Based on wind-mask/aw-tauri@435b3b6c
// Lightweight mode: tray + server, no Tauri WebView (~400 MB saved on Linux).

use crate::manager;
use log::{error, info, warn};
use std::{io::Cursor, path::Path, sync::mpsc, thread};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon, TrayIconBuilder,
};

enum MiniEvent {
    Menu(MenuEvent),
    Manager(manager::ManagerEvent),
    ServerFailed(String),
}

pub fn run() {
    let cli_args = crate::get_cli_args();

    let user_config = crate::get_config();

    let (dashboard_url, server_state, aw_config) =
        match crate::prepare_aw_server(user_config, cli_args) {
            Ok(server) => server,
            Err(message) => {
                error!("{}", message);
                eprintln!("{}", message);
                std::process::exit(1);
            }
        };
    let server_port = aw_config.port;
    let rocket_handle = tauri::async_runtime::spawn(
        aw_server::endpoints::build_rocket(server_state, aw_config).launch(),
    );
    info!("Running aw-tauri mini mode at {}", dashboard_url.as_str());

    let event_loop = EventLoopBuilder::<MiniEvent>::with_user_event().build();
    let server_proxy = event_loop.create_proxy();
    tauri::async_runtime::spawn(async move {
        match rocket_handle.await {
            Ok(Err(e)) => {
                error!("Server exited with error: {e:?}");
                let _ = server_proxy.send_event(MiniEvent::ServerFailed(format!("{e:?}")));
            }
            Err(join_err) => {
                error!("Rocket task panicked: {join_err:?}");
                let _ = server_proxy.send_event(MiniEvent::ServerFailed(format!("{join_err:?}")));
            }
            Ok(Ok(_)) => {} // clean shutdown — event loop is likely already exiting
        }
    });
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(MiniEvent::Menu(event));
    }));

    let (manager_tx, manager_rx) = mpsc::channel();
    let manager_proxy = event_loop.create_proxy();
    thread::spawn(move || {
        for event in manager_rx {
            if manager_proxy.send_event(MiniEvent::Manager(event)).is_err() {
                break;
            }
        }
    });

    let manager_state = manager::start_manager_with_events(server_port, manager_tx);
    let mut modules = {
        let state = manager_state
            .lock()
            .expect("Failed to acquire manager_state lock");
        state.modules_snapshot()
    };

    let mut tray_icon: Option<TrayIcon> = None;
    let mut first_run_notified = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) if tray_icon.is_none() => {
                tray_icon = Some(create_tray_icon(&modules));
                if !first_run_notified && *crate::is_first_run() {
                    show_notification(
                        "Aw-Tauri",
                        "Welcome to Aw-Tauri! Use the tray icon to open the dashboard.",
                    );
                    first_run_notified = true;
                }
            }
            Event::UserEvent(MiniEvent::Menu(event)) => {
                let id = event.id.0;
                match id.as_str() {
                    "open" => open_dashboard(dashboard_url.as_str()),
                    "quit" => {
                        if let Ok(mut state) = manager_state.lock() {
                            state.stop_modules();
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                    "config_folder" => {
                        let config_path = crate::get_config_path();
                        let config_dir = config_path.parent().unwrap_or(&config_path);
                        open_path(config_dir);
                    }
                    "log_folder" => {
                        let log_path = crate::logging::get_log_path();
                        let log_dir = log_path.parent().unwrap_or(&log_path);
                        open_path(log_dir);
                    }
                    _ => {
                        if let Some(module_name) = id.strip_prefix("module:") {
                            if let Ok(mut state) = manager_state.lock() {
                                state.handle_system_click(module_name);
                            }
                        }
                    }
                }
            }
            Event::UserEvent(MiniEvent::ServerFailed(msg)) => {
                show_notification("ActivityWatch Error", &format!("Server failed: {msg}"));
                if let Ok(mut state) = manager_state.lock() {
                    state.stop_modules();
                }
                std::process::exit(1);
            }
            Event::UserEvent(MiniEvent::Manager(event)) => match event {
                manager::ManagerEvent::ModulesChanged { modules: changed } => {
                    modules = (*changed).clone();
                    if let Some(tray_icon) = &tray_icon {
                        update_tray_menu(tray_icon, &modules);
                    }
                }
                manager::ManagerEvent::Notification { title, message } => {
                    show_notification(&title, &message);
                }
            },
            _ => {}
        }
    });
}

fn create_tray_icon(modules: &manager::ModulesSnapshot) -> TrayIcon {
    let menu = build_tray_menu(modules).expect("Failed to create mini tray menu");
    let icon = load_tray_icon().expect("Failed to load mini tray icon");

    #[allow(unused_mut)] // only reassigned on Linux, below
    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("ActivityWatch")
        .with_menu_on_left_click(true);

    #[cfg(target_os = "linux")]
    {
        builder = builder.with_temp_dir_path(crate::dirs::get_runtime_dir().join("tray-icon"));
    }

    builder.build().expect("Failed to create mini tray")
}

fn update_tray_menu(tray_icon: &TrayIcon, modules: &manager::ModulesSnapshot) {
    match build_tray_menu(modules) {
        Ok(menu) => tray_icon.set_menu(Some(Box::new(menu))),
        Err(e) => error!("Failed to update mini tray menu: {e}"),
    }
}

fn build_tray_menu(modules: &manager::ModulesSnapshot) -> Result<Menu, Box<dyn std::error::Error>> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Dashboard", true, None);
    menu.append(&open)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let modules_submenu = Submenu::with_id("modules", "Modules", true);
    // Started modules first, alphabetically, each with a checkbox.
    for (module, run_state) in modules {
        if let Some(running) = run_state {
            let module_menu =
                CheckMenuItem::with_id(module_menu_id(module), module, true, *running, None);
            modules_submenu.append(&module_menu)?;
        }
    }
    // Then discovered modules that have never been started, alphabetically.
    for (module, run_state) in modules {
        if run_state.is_none() {
            let module_menu = MenuItem::with_id(module_menu_id(module), module, true, None);
            modules_submenu.append(&module_menu)?;
        }
    }
    menu.append(&modules_submenu)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let config_folder = MenuItem::with_id("config_folder", "Open config folder", true, None);
    let log_folder = MenuItem::with_id("log_folder", "Open log folder", true, None);
    menu.append(&config_folder)?;
    menu.append(&log_folder)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let quit = MenuItem::with_id("quit", "Quit ActivityWatch", true, None);
    menu.append(&quit)?;

    Ok(menu)
}

fn module_menu_id(module: &str) -> String {
    format!("module:{module}")
}

fn load_tray_icon() -> Result<Icon, Box<dyn std::error::Error>> {
    let icon_bytes = include_bytes!("../icons/32x32.png");
    let decoder = png::Decoder::new(Cursor::new(icon_bytes));
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer)?;
    let rgba = buffer[..info.buffer_size()].to_vec();

    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("mini tray icon must be an 8-bit RGBA PNG".into());
    }

    Ok(Icon::from_rgba(rgba, info.width, info.height)?)
}

fn open_dashboard(url: &str) {
    if let Err(e) = open::that_detached(url) {
        warn!("Failed to open dashboard: {e}");
    }
}

fn open_path(path: &Path) {
    if let Err(e) = open::that_detached(path) {
        warn!("Failed to open path {}: {e}", path.display());
    }
}

fn show_notification(title: &str, message: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(title)
        .body(message)
        .show()
    {
        warn!("Failed to show notification: {e}");
    }
}
