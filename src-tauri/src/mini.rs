// Based on wind-mask/aw-tauri@435b3b6c
// Lightweight mode: tray + server, no Tauri WebView (~400 MB saved on Linux).

use crate::manager;
use log::{error, info, warn};
use std::{
    collections::{BTreeSet, HashMap},
    io::Cursor,
    path::Path,
    sync::{mpsc, Arc},
    thread,
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon, TrayIconBuilder,
};

/// Matches `identifier` in tauri.conf.json.
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_ID: &str = "net.activitywatch.tauri";

enum MiniEvent {
    Menu(MenuEvent),
    Manager(manager::ManagerEvent),
    /// Rocket shut down cleanly, e.g. on SIGTERM.
    ServerStopped,
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

    // Pick the notification sender up front. Otherwise notify-rust resolves one on the first
    // notification by running an AppleScript lookup for an app named "use_default", on the main
    // thread, which can spin at full CPU indefinitely and freeze the tray.
    #[cfg(target_os = "macos")]
    if let Err(e) = notify_rust::set_application(MACOS_BUNDLE_ID) {
        warn!("Failed to set notification sender to {MACOS_BUNDLE_ID}: {e}");
    }

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
            Ok(Ok(_)) => {
                // Rocket handles SIGINT/SIGTERM (and SIGHUP on Unix) by shutting the server down.
                // If the user quit, the event loop is already gone and this send fails harmlessly.
                let _ = server_proxy.send_event(MiniEvent::ServerStopped);
            }
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
        Arc::new(state.modules_snapshot())
    };

    let mut tray: Option<MiniTray> = None;
    let mut first_run_notified = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) if tray.is_none() => {
                tray = Some(MiniTray::new(&modules));
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
            Event::UserEvent(MiniEvent::ServerStopped) => {
                info!("Server shut down, exiting");
                if let Ok(mut state) = manager_state.lock() {
                    state.stop_modules();
                }
                *control_flow = ControlFlow::Exit;
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
                    modules = changed;
                    if let Some(tray) = &mut tray {
                        tray.update(&modules);
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

/// The mini-mode tray icon plus what's needed to update its menu in place.
struct MiniTray {
    icon: TrayIcon,
    /// Check items for modules in the running group, keyed by module name.
    module_items: HashMap<String, CheckMenuItem>,
    /// Modules that had been started at least once when the menu was last built. The menu only
    /// needs rebuilding when this changes; otherwise only checkmarks change.
    running_keys: BTreeSet<String>,
}

impl MiniTray {
    fn new(modules: &manager::ModulesSnapshot) -> Self {
        let (menu, module_items) =
            build_tray_menu(modules).expect("Failed to create mini tray menu");
        MiniTray {
            icon: create_tray_icon(menu),
            module_items,
            running_keys: running_keys(modules),
        }
    }

    fn update(&mut self, modules: &manager::ModulesSnapshot) {
        let running_keys = running_keys(modules);
        if running_keys == self.running_keys {
            // Same running group, so the menu order is unchanged: only sync checkmarks.
            for (name, run_state) in modules {
                if let (Some(item), Some(running)) = (self.module_items.get(name), run_state) {
                    item.set_checked(*running);
                }
            }
            return;
        }
        match build_tray_menu(modules) {
            Ok((menu, module_items)) => {
                self.icon.set_menu(Some(Box::new(menu)));
                self.module_items = module_items;
                self.running_keys = running_keys;
            }
            Err(e) => error!("Failed to update mini tray menu: {e}"),
        }
    }
}

/// Modules that have been started at least once, i.e. the tray's top group.
fn running_keys(modules: &manager::ModulesSnapshot) -> BTreeSet<String> {
    modules
        .iter()
        .filter(|(_, run_state)| run_state.is_some())
        .map(|(name, _)| name.clone())
        .collect()
}

fn create_tray_icon(menu: Menu) -> TrayIcon {
    let icon = load_tray_icon().expect("Failed to load mini tray icon");

    #[allow(unused_mut)] // only reassigned on Linux, below
    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip(crate::profile::tray_tooltip(&crate::get_cli_args().profile))
        .with_menu_on_left_click(true);

    #[cfg(target_os = "linux")]
    {
        builder = builder.with_temp_dir_path(crate::dirs::get_runtime_dir().join("tray-icon"));
    }

    builder.build().expect("Failed to create mini tray")
}

type ModuleItems = HashMap<String, CheckMenuItem>;

fn build_tray_menu(
    modules: &manager::ModulesSnapshot,
) -> Result<(Menu, ModuleItems), Box<dyn std::error::Error>> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Dashboard", true, None);
    menu.append(&open)?;
    menu.append(&PredefinedMenuItem::separator())?;

    let mut module_items = HashMap::new();
    let modules_submenu = Submenu::with_id("modules", "Modules", true);
    // Started modules first, alphabetically, each with a checkbox.
    for (module, run_state) in modules {
        if let Some(running) = run_state {
            let module_menu =
                CheckMenuItem::with_id(module_menu_id(module), module, true, *running, None);
            modules_submenu.append(&module_menu)?;
            module_items.insert(module.clone(), module_menu);
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

    Ok((menu, module_items))
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
