/// A process manager for ActivityWatch
///
/// Used to start, stop and manage the lifecycle modules like aw-watcher-afk and aw-watcher-window.
/// A module is a process that runs in the background and sends events to the ActivityWatch server.
///
/// The manager is responsible for starting and stopping the modules, and for keeping track of
/// their state.
///
/// If a module crashes, the manager will notify the user and ask if they want to restart it.
use log::{debug, error, info};

#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex,
};
use std::time::Duration;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use std::{env, fs, thread};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, SubmenuBuilder};

#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

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
    pub modules_in_path: BTreeSet<String>,
    pub modules_pid: HashMap<String, u32>,
    pub modules_restart_count: HashMap<String, u32>,
    pub modules_args: HashMap<String, Option<Vec<String>>>,
    pub modules_menu_set: bool,
}

impl ManagerState {
    fn new(tx: Sender<ModuleMessage>) -> ManagerState {
        ManagerState {
            tx,
            modules_running: BTreeMap::new(),
            modules_in_path: get_modules_in_path(),
            modules_pid: HashMap::new(),
            modules_restart_count: HashMap::new(),
            modules_args: HashMap::new(),
            modules_menu_set: false,
        }
    }
    fn started_module(&mut self, name: &str, pid: u32, args: Option<Vec<String>>) {
        info!("Started module: {name}");
        self.modules_running.insert(name.to_string(), true);
        self.modules_pid.insert(name.to_string(), pid);
        self.modules_args.insert(name.to_string(), args);
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

        let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
            .expect("failed to create open menu item");
        let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
            .expect("failed to create quit menu item");

        let mut modules_submenu_builder = SubmenuBuilder::new(app, "Modules");
        for (module, running) in self.modules_running.iter() {
            let label = module;
            let module_menu =
                CheckMenuItem::with_id(app, module, label, true, *running, None::<&str>)
                    .expect("failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }

        for module in self.modules_in_path.iter() {
            if !self.modules_running.contains_key(module) {
                let module_menu = MenuItem::with_id(app, module, module, true, None::<&str>)
                    .expect("failed to create module menu item");
                modules_submenu_builder = modules_submenu_builder.item(&module_menu);
            }
        }

        let module_submenu = modules_submenu_builder
            .build()
            .expect("failed to create module submenu");
        let menu = Menu::with_items(app, &[&open, &module_submenu, &quit])
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
            start_module_thread(name.to_string(), args.cloned(), self.tx.clone());
        }
    }
    pub fn stop_module(&self, name: &str) {
        if let Some(pid) = self.modules_pid.get(name) {
            if let Err(e) = send_sigterm(*pid) {
                error!("Failed to send SIGTERM to module {name}: {e}");
            } else {
                debug!("Sent SIGTERM to module: {name}");
            }
        }
    }
    pub fn stop_modules(&self) {
        for (name, _pid) in self.modules_pid.iter() {
            self.stop_module(name);
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
    // Get the process ID of the child process
    let pid = pid as DWORD;

    // Send SIGTERM signal to the process
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    } else {
        return Ok(());
    }
}
pub fn start_manager() -> Arc<Mutex<ManagerState>> {
    let (tx, rx) = channel();
    let state = Arc::new(Mutex::new(ManagerState::new(tx.clone())));

    // Start the modules
    let config = get_config();
    for module_config in config.autostart_modules.iter() {
        let args = if module_config.args.is_empty() {
            None
        } else {
            // Split args string on whitespace, preserving quoted arguments
            Some(shell_words::split(&module_config.args).unwrap_or_default())
        };
        state
            .lock()
            .unwrap()
            .start_module(&module_config.name, args.as_ref());
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
                        let restart_count = state
                            .modules_restart_count
                            .entry(name_clone.clone())
                            .or_insert(0);
                        if *restart_count < 3 {
                            *restart_count += 1;
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

fn start_module_thread(name: String, custom_args: Option<Vec<String>>, tx: Sender<ModuleMessage>) {
    thread::spawn(move || {
        // Start the child process
        let port_string = get_config().defaults.port.to_string();
        let mut command = Command::new(&name);

        // Use custom args if provided, otherwise use default port arg
        if let Some(ref args) = custom_args {
            command.args(args);
        } else {
            command.args(["--port", port_string.as_str()]);
        }

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
fn get_modules_in_path() -> BTreeSet<String> {
    let excluded = ["awk", "aw-tauri", "aw-client", "aw-cli", "aw-qt"];
    let config = crate::get_config();

    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    if !paths.contains(&config.defaults.discovery_path) {
        // add to the front of the path list
        paths.insert(0, config.defaults.discovery_path.to_owned());
    }

    // Create new PATH-like string
    let new_paths = env::join_paths(paths).unwrap_or_default();

    env::split_paths(&new_paths)
        .flat_map(|path| fs::read_dir(path).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let is_executable = (metadata.is_file() || metadata.is_symlink())
                && metadata.permissions().mode() & 0o111 != 0;
            if !is_executable {
                return None;
            }

            entry
                .file_name()
                .to_str()
                .map(|s| s.to_string())
                .filter(|name: &String| name.starts_with("aw") && !name.contains("."))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name: &String| !excluded.contains(&name.as_str()))
        .collect()
}

#[cfg(windows)]
fn get_modules_in_path() -> BTreeSet<String> {
    let excluded = ["aw-tauri", "aw-client", "aw-cli", "aw-qt"];

    // Get the discovery path from config
    let config = crate::get_config();

    // Get the current PATH
    let path = env::var_os("PATH").unwrap_or_default();
    let mut paths = env::split_paths(&path).collect::<Vec<_>>();

    // Add discovery path if not already in PATH
    if !paths.contains(&config.defaults.discovery_path) {
        paths.push(config.defaults.discovery_path.clone());
    }

    // Create new PATH-like string
    let new_paths = env::join_paths(paths).unwrap_or_default();

    // Use the combined paths to find modules
    env::split_paths(&new_paths)
        .flat_map(|path| fs::read_dir(path).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() && path.extension().map_or(false, |ext| ext == "exe") {
                path.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| !excluded.contains(&name.as_str()))
        .collect()
}
