//! A process manager for ActivityWatch
//!
//! Used to start, stop and manage the lifecycle modules like aw-watcher-afk and aw-watcher-window.
//! A module is a process that runs in the background and sends events to the ActivityWatch server.
//!
//! The manager is responsible for starting and stopping the modules, and for keeping track of
//! their state.
//!
//! If a module crashes, the manager will notify the user and ask if they want to restart it.

#[cfg(unix)]
use {
    nix::sys::signal::{self, Signal},
    nix::unistd::Pid,
    std::os::unix::fs::PermissionsExt,
};
#[cfg(windows)]
use {
    std::os::windows::process::CommandExt,
    winapi::shared::minwindef::{DWORD, FALSE},
    winapi::um::handleapi::CloseHandle,
    winapi::um::processthreadsapi::{OpenProcess, TerminateProcess},
    winapi::um::winbase::CREATE_NO_WINDOW,
    winapi::um::winnt::PROCESS_TERMINATE,
};

use glob::glob;
use log::{debug, error, info};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::time::Duration;
use std::{env, fs, thread};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::{get_app_handle, get_config, get_tray_id, HANDLE_CONDVAR};

#[derive(Debug)]
pub enum ModuleMessage {
    Started {
        name: String,
        pid: u32,
        args: Option<Vec<String>>,
    },
    Stopped {
        name: String,
        output: std::process::Output,
    },
    Init {},
}

#[derive(Debug)]
pub struct ManagerState {
    tx: Sender<ModuleMessage>,
    pub modules_running: BTreeMap<String, bool>,
    pub modules_discovered: BTreeMap<String, PathBuf>,
    pub modules_pid: HashMap<String, u32>,
    pub modules_restart_count: HashMap<String, u32>,
    pub modules_pending_shutdown: HashMap<String, bool>,
    pub modules_args: HashMap<String, Option<Vec<String>>>,
    pub modules_menu_set: bool,
}

impl ManagerState {
    fn new(tx: Sender<ModuleMessage>) -> ManagerState {
        ManagerState {
            tx,
            //TODO: merge some of these maps into a single struct
            modules_running: BTreeMap::new(),
            modules_discovered: discover_modules(),
            modules_pid: HashMap::new(),
            modules_restart_count: HashMap::new(),
            modules_pending_shutdown: HashMap::new(),
            modules_args: HashMap::new(),
            modules_menu_set: false,
        }
    }
    fn started_module(&mut self, name: &str, pid: u32, args: Option<Vec<String>>) {
        info!("Started module: {name}");
        self.modules_running.insert(name.to_string(), true);
        self.modules_pid.insert(name.to_string(), pid);
        self.modules_args.insert(name.to_string(), args);
        self.modules_pending_shutdown.remove(name);
        debug!("Running modules: {:?}", self.modules_running);
        self.update_tray_menu();
    }
    fn stopped_module(&mut self, name: &str) {
        info!("Stopped module: {name}");
        self.modules_running.insert(name.to_string(), false);
        self.modules_pid.remove(name);
        self.update_tray_menu();
    }
    fn update_tray_menu(&mut self) {
        let (lock, cvar) = &*HANDLE_CONDVAR;
        let mut state = lock.lock().unwrap();

        debug!("Attempting to get app handle");
        while !*state {
            state = cvar.wait(state).unwrap();
        }
        debug!("Condition variable set");
        let app = &*get_app_handle().lock().expect("failed to get app handle");
        debug!("App handle acquired");

        let open = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)
            .expect("failed to create open menu item");
        let quit = MenuItem::with_id(app, "quit", "Quit ActivityWatch", true, None::<&str>)
            .expect("failed to create quit menu item");

        let mut modules_submenu_builder = SubmenuBuilder::new(app, "Modules");
        for (module, running) in self.modules_running.iter() {
            let label = module;
            let module_menu =
                CheckMenuItem::with_id(app, module, label, true, *running, None::<&str>)
                    .expect("failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }

        for module_name in self.modules_discovered.keys() {
            if !self.modules_running.contains_key(module_name) {
                let module_menu =
                    MenuItem::with_id(app, module_name, module_name, true, None::<&str>)
                        .expect("failed to create module menu item");
                modules_submenu_builder = modules_submenu_builder.item(&module_menu);
            }
        }

        let module_submenu = modules_submenu_builder
            .build()
            .expect("failed to create module submenu");
        let config_folder = MenuItem::with_id(
            app,
            "config_folder",
            "Open config folder",
            true,
            None::<&str>,
        )
        .expect("failed to create config folder menu item");

        let log_folder =
            MenuItem::with_id(app, "log_folder", "Open log folder", true, None::<&str>)
                .expect("failed to create log folder menu item");
        let separator = PredefinedMenuItem::separator(app).expect("failed to create separator");
        let menu = Menu::with_items(
            app,
            &[
                &open,
                &separator,
                &module_submenu,
                &separator,
                &config_folder,
                &log_folder,
                &separator,
                &quit,
            ],
        )
        .expect("failed to create tray menu");

        let tray_id = get_tray_id();
        app.tray_by_id(tray_id)
            .expect("failed to get tray by id")
            .set_menu(Some(menu))
            .unwrap();
        println!("set tray menu");
    }
    pub fn start_module(&self, name: &str, args: Option<&Vec<String>>) {
        if !self.is_module_running(name) {
            if let Some(path) = self.modules_discovered.get(name) {
                start_module_thread(
                    name.to_string(),
                    path.clone(),
                    args.cloned(),
                    self.tx.clone(),
                );
            } else {
                error!("Module {name} not found in PATH");
            }
        }
    }
    pub fn stop_module(&mut self, name: &str) {
        if let Some(pid) = self.modules_pid.get(name) {
            // add to pending shutdown to prevent restart
            self.modules_pending_shutdown.insert(name.to_string(), true);
            if let Err(e) = send_sigterm(*pid) {
                error!("Failed to send SIGTERM to module {name}: {e}");
            } else {
                debug!("Sent SIGTERM to module: {name}");
            }
        }
    }
    pub fn stop_modules(&mut self) {
        let module_names: Vec<String> = self.modules_pid.keys().cloned().collect();
        for name in module_names {
            self.stop_module(&name);
        }
    }
    pub fn handle_system_click(&mut self, name: &str) {
        if self.is_module_running(name) {
            self.stop_module(name);
        } else {
            self.start_module(name, None);
        }
    }
    fn is_module_running(&self, name: &str) -> bool {
        *self.modules_running.get(name).unwrap_or(&false)
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<(), nix::Error> {
    let pid = Pid::from_raw(pid as i32);
    let res = signal::kill(pid, Signal::SIGTERM);
    if let Err(e) = res {
        Err(e)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn send_sigterm(pid: u32) -> Result<(), std::io::Error> {
    let pid = pid as DWORD;

    // Open the process with terminate permission
    let process_handle = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };

    if process_handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    // Terminate the process with exit code 1
    let result = unsafe { TerminateProcess(process_handle, 1) };

    // Close the process handle
    unsafe { CloseHandle(process_handle) };

    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub fn start_manager() -> Arc<Mutex<ManagerState>> {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new(tx.clone())));

    // Start the modules
    let config = get_config();
    for module_entry in config.autostart.modules.iter() {
        let name = module_entry.name();
        let args_str = module_entry.args();

        let args = if args_str.is_empty() {
            None
        } else {
            // Split args string on whitespace, preserving quoted arguments
            Some(shell_words::split(args_str).unwrap_or_default())
        };
        state.lock().unwrap().start_module(name, args.as_ref());
    }

    // populate the tray menu if not yet already done
    let modules_menu_set = state.lock().unwrap().modules_menu_set;
    if !modules_menu_set {
        tx.send(ModuleMessage::Init {}).unwrap();
    }

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone);
    });
    state
}

fn handle(rx: Receiver<ModuleMessage>, state: Arc<Mutex<ManagerState>>) {
    loop {
        let msg = rx.recv().unwrap();
        let state_clone = Arc::clone(&state);
        let state = &mut state.lock().unwrap();
        match msg {
            ModuleMessage::Started { name, pid, args } => {
                state.started_module(&name, pid, args);
            }
            ModuleMessage::Stopped { name, output } => {
                state.stopped_module(&name);
                let name_clone = name.clone();
                if output.status.success() {
                    info!("Module {name} exited successfully");
                } else {
                    error!("Module {name} exited with error status");
                    thread::spawn(move || {
                        thread::sleep(Duration::from_secs(1));
                        let state = &mut state_clone.lock().unwrap();
                        let restart_count =
                            state.modules_restart_count.get(&name_clone).unwrap_or(&0);

                        let pending_shutdown = state
                            .modules_pending_shutdown
                            .get(&name_clone)
                            .unwrap_or(&false);

                        if *pending_shutdown {
                            return;
                        }
                        if *restart_count < 3 {
                            let new_count = *restart_count + 1;
                            state
                                .modules_restart_count
                                .insert(name_clone.clone(), new_count);
                            // Get the stored arguments for this module
                            let stored_args =
                                state.modules_args.get(&name_clone).cloned().flatten();
                            state.start_module(&name_clone, stored_args.as_ref());
                            let app = &*get_app_handle().lock().expect("failed to get app handle");

                            app.dialog()
                                .message(format!("{name_clone} crashed. Restarting..."))
                                .kind(MessageDialogKind::Error)
                                .title("Aw-Tauri")
                                .show(|_| {});
                            error!("Module {name_clone} crashed and is being restarted");
                        } else {
                            let app = &*get_app_handle().lock().expect("failed to get app handle");

                            app.dialog()
                                .message(format!(
                                    "{name_clone} keeps on crashing. Restart limit reached."
                                ))
                                .kind(MessageDialogKind::Error)
                                .title("Warning")
                                .show(|_| {});
                            error!("Module {name_clone} exceeded crash restart limit");
                        }
                    });

                    debug!(
                        "Module {name} stdout: {}",
                        String::from_utf8_lossy(&output.stdout)
                    );
                    error!(
                        "Module {name} stderr: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
            ModuleMessage::Init {} => state.update_tray_menu(),
        }
    }
}

fn start_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Start the child process
        let port_string = get_config().port.to_string();
        let mut command = Command::new(&path);

        // Use custom args if provided, otherwise use default port arg
        if let Some(ref args) = custom_args {
            command.args(args);
        } else {
            command.args(["--port", port_string.as_str()]);
        }

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let child = command.stdout(std::process::Stdio::piped()).spawn();

        if let Err(e) = child {
            error!("Failed to start module {name}: {e}");
            return;
        }

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child.as_ref().unwrap().id(),
            args: custom_args,
        })
        .unwrap();

        // Wait for the child to exit
        let output = child
            .unwrap()
            .wait_with_output()
            .expect("failed to wait on child");

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .unwrap();
    });
}

#[cfg(unix)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "awk",
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    // Create new PATH-like string
    let new_paths = env::join_paths(paths).unwrap_or_default();

    env::split_paths(&new_paths)
        .flat_map(|path| {
            let pattern = path.join("**").join("aw*").to_string_lossy().to_string();

            // Use glob to find all matching files (with recursive=true)
            glob(&pattern)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
        })
        .filter_map(|path| {
            let file_name = path.file_name()?.to_str()?.to_string();

            if file_name.contains(".") || excluded.contains(&file_name.as_str()) {
                return None;
            }

            match fs::metadata(&path) {
                Ok(metadata) => {
                    let is_executable = (metadata.is_file() || metadata.is_symlink())
                        && metadata.permissions().mode() & 0o111 != 0;

                    if is_executable {
                        Some((file_name, path))
                    } else {
                        None
                    }
                }
                Err(_) => None,
            }
        })
        .collect()
}

#[cfg(windows)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "awk",
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
    ];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // check each path in discovery_paths and add it to the start of the paths list if it's not already there
    for path in config.discovery_paths.iter() {
        if !paths.contains(path) {
            paths.insert(0, path.to_owned());
        }
    }

    let new_paths = env::join_paths(paths).unwrap_or_default();

    env::split_paths(&new_paths)
        .flat_map(|path| {
            let pattern = path.join("**").join("aw*").to_string_lossy().to_string();

            // Use glob to find all matching files (with recursive=true)
            glob(&pattern)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
        })
        .filter_map(|path| {
            let file_name = path.file_name()?.to_str()?.to_string();

            let name = file_name.strip_suffix(".exe")?.to_lowercase();

            if excluded.contains(&name.as_str()) {
                return None;
            } else {
                Some((name, path))
            }
        })
        .collect()
}
