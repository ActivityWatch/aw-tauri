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

use log::{debug, error, info, trace};
use std::collections::{BTreeMap, HashMap, HashSet};
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
use std::io::{BufRead, BufReader};
use tauri_plugin_notification::NotificationExt;

#[derive(Debug)]
enum ModuleMessage {
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
        let mut state = lock.lock().expect("Failed to acquire manager_state lock");

        debug!("Attempting to get app handle");
        while !*state {
            state = cvar
                .wait(state)
                .expect("Failed to wait on condition variable");
        }
        debug!("Condition variable set");
        let app = &*get_app_handle().lock().expect("Failed to get app handle");
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
                    .expect("Failed to create module menu item");
            modules_submenu_builder = modules_submenu_builder.item(&module_menu);
        }

        for module_name in self.modules_discovered.keys() {
            if !self.modules_running.contains_key(module_name) {
                let module_menu =
                    MenuItem::with_id(app, module_name, module_name, true, None::<&str>)
                        .expect("Failed to create module menu item");
                modules_submenu_builder = modules_submenu_builder.item(&module_menu);
            }
        }

        let module_submenu = modules_submenu_builder
            .build()
            .expect("Failed to create module submenu");
        let config_folder = MenuItem::with_id(
            app,
            "config_folder",
            "Open config folder",
            true,
            None::<&str>,
        )
        .expect("Failed to create config folder menu item");

        let log_folder =
            MenuItem::with_id(app, "log_folder", "Open log folder", true, None::<&str>)
                .expect("Failed to create log folder menu item");
        let separator = PredefinedMenuItem::separator(app).expect("Failed to create separator");
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
        .expect("Failed to create tray menu");

        let tray_id = get_tray_id();
        app.tray_by_id(tray_id)
            .expect("Failed to get tray by id")
            .set_menu(Some(menu))
            .expect("Failed to set tray menu");
        trace!("set tray menu");
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
        state
            .lock()
            .expect("Failed to acquire manager_state lock")
            .start_module(name, args.as_ref());
    }

    // populate the tray menu if not yet already done
    let modules_menu_set = state
        .lock()
        .expect("Failed to acquire manager_state lock")
        .modules_menu_set;
    if !modules_menu_set {
        tx.send(ModuleMessage::Init {})
            .expect("Failed to send \"Module Init\" message");
    }

    let state_clone = Arc::clone(&state);
    thread::spawn(move || {
        handle(rx, state_clone);
    });
    state
}

fn handle(rx: Receiver<ModuleMessage>, state: Arc<Mutex<ManagerState>>) {
    loop {
        let msg = rx.recv().expect("Failed to receive Module message");
        let state_clone = Arc::clone(&state);
        let state = &mut state.lock().expect("Failed to acquire manager_state lock");
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
                        let state = &mut state_clone
                            .lock()
                            .expect("Failed to acquire manager_state lock");
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
                            let app = &*get_app_handle().lock().expect("Failed to get app handle");

                            app.dialog()
                                .message(format!("{name_clone} crashed. Restarting..."))
                                .kind(MessageDialogKind::Warning)
                                .title("Warning")
                                .show(|_| {});
                            error!("Module {name_clone} crashed and is being restarted");
                        } else {
                            let app = &*get_app_handle().lock().expect("Failed to get app handle");

                            app.dialog()
                                .message(format!(
                                    "{name_clone} keeps on crashing. Restart limit reached."
                                ))
                                .kind(MessageDialogKind::Warning)
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
    // Special handling for aw-notify module
    if name == "aw-notify" {
        info!("Using special aw-notify handler for module: {name}");
        start_notify_module_thread(name, path, custom_args, tx);
        return;
    }

    thread::spawn(move || {
        // Start the child process
        let mut command = Command::new(&path);

        // Use custom args if provided, otherwise only pass port arg if it's not the default (5600)
        if let Some(ref args) = custom_args {
            command.args(args);
        } else if get_config().port != 5600 {
            command.args(["--port", get_config().port.to_string().as_str()]);
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
            pid: child.as_ref().expect("Failed to get child PID").id(),
            args: custom_args,
        })
        .expect("Failed to send Module Started message");

        // Wait for the child to exit
        let output = child
            .expect("Failed to create child process")
            .wait_with_output()
            .expect("Failed to wait on child process");

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

fn start_notify_module_thread(
    name: String,
    path: PathBuf,
    custom_args: Option<Vec<String>>,
    tx: Sender<ModuleMessage>,
) {
    thread::spawn(move || {
        // Start the child process with --output-only flag
        let mut command = Command::new(&path);

        // Always add --output-only flag for aw-notify
        let mut args = vec!["--output-only".to_string()];

        // Add port argument if not default (5600)
        if get_config().port != 5600 {
            args.push("--port".to_string());
            args.push(get_config().port.to_string());
        }

        // Add any custom args
        if let Some(ref custom) = custom_args {
            args.extend_from_slice(custom);
        }

        command.args(&args);

        // Set creation flags on Windows to hide console window
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = match command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("No such option: --output-only") {
                    info!("aw-notify module doesn't support --output-only, falling back to default behavior");
                    // Fallback to default module handler
                    start_module_thread(name, path, custom_args, tx);
                    return;
                } else {
                    error!("Failed to start module {name}: {e}");
                    return;
                }
            }
        };

        // Send a message to the manager that the module has started
        tx.send(ModuleMessage::Started {
            name: name.to_string(),
            pid: child.id(),
            args: Some(args),
        })
        .expect("Failed to send module started message");

        // Read output continuously and parse notifications
        let stdout = child.stdout.take().expect("Failed to get stdout");
        let reader = BufReader::new(stdout);

        let mut in_notification = false;
        let mut notification_content = Vec::new();

        for line in reader.lines() {
            match line {
                Ok(line_content) => {
                    // Check for notification boundaries (exactly 50 dashes)
                    if line_content == "-".repeat(50) {
                        if in_notification {
                            // End of notification - send it
                            if !notification_content.is_empty() {
                                let content = notification_content.join("\n");
                                send_notification(&content);
                                notification_content.clear();
                            }
                            in_notification = false;
                        } else {
                            // Start of notification
                            in_notification = true;
                        }
                    } else if in_notification && !line_content.trim().is_empty() {
                        // Collect notification content
                        notification_content.push(line_content.clone());
                    }
                    // Debug log aw-notify output (won't show at Info level)
                    debug!("aw-notify output: {}", line_content);
                }
                Err(e) => {
                    error!("Error reading aw-notify output: {}", e);
                    break;
                }
            }
        }

        // Wait for the child to exit
        let output = child.wait_with_output().expect("Failed to wait on child");

        // Send the process output to the manager
        tx.send(ModuleMessage::Stopped {
            name: name.to_string(),
            output,
        })
        .expect("Failed to send module stopped message");
    });
}

fn send_notification(content: &str) {
    // Get app handle and send notification
    if let Ok(app_handle_guard) = get_app_handle().lock() {
        let app_handle = &*app_handle_guard;
        let result = app_handle
            .notification()
            .builder()
            .title("ActivityWatch")
            .body(content)
            .show();

        match result {
            Ok(_) => {
                trace!(
                    "Sent notification: {}",
                    content.lines().next().unwrap_or("")
                );
            }
            Err(e) => {
                error!("Failed to send notification: {}", e);
            }
        }
    } else {
        error!("Failed to get app handle lock for notification");
    }
}

#[cfg(unix)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
        "aw-tauri",
        "aw-client",
        "aw-cli",
        "aw-qt",
        "aw-server",
        "aw-server-rust",
        "aw-watcher-window-macos",
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

    // Build a set of paths to search
    let mut found_modules = BTreeMap::new();
    let mut visited_dirs = HashSet::new();

    // Create a stack of directories to search, starting with PATH entries
    let mut dirs_to_search: Vec<PathBuf> = env::split_paths(&new_paths).collect();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        if !visited_dirs.insert(dir.canonicalize().unwrap_or(dir.clone())) {
            continue;
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();

                // Skip if not a file or directory
                if let Ok(metadata) = fs::metadata(&path) {
                    let file_name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(name) => name.to_string(),
                        None => continue,
                    };

                    // Process only items starting with "aw-"
                    if !file_name.starts_with("aw-") {
                        continue;
                    }

                    // If it's a directory starting with "aw-", add to search stack
                    if metadata.is_dir() {
                        dirs_to_search.push(path);
                    }
                    // If it's an executable file
                    else if metadata.is_file() || metadata.is_symlink() {
                        // Skip if has extension or is excluded
                        if file_name.contains(".") || excluded.contains(&file_name.as_str()) {
                            continue;
                        }

                        // Check if executable
                        let is_executable = metadata.permissions().mode() & 0o111 != 0;
                        if is_executable {
                            found_modules.insert(file_name, path);
                        }
                    }
                }
            }
        }
    }

    debug!(
        "Discovered modules: {:?}",
        found_modules.keys().collect::<Vec<_>>()
    );
    found_modules
}

#[cfg(windows)]
fn discover_modules() -> BTreeMap<String, PathBuf> {
    let excluded = [
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

    // Build a set of paths to search
    let mut found_modules = BTreeMap::new();
    let mut visited_dirs = HashSet::new();

    // Create a stack of directories to search, starting with PATH entries
    let mut dirs_to_search: Vec<PathBuf> = env::split_paths(&new_paths).collect();

    // Process directories in depth-first order
    while let Some(dir) = dirs_to_search.pop() {
        // Skip if already visited
        if !visited_dirs.insert(dir.clone()) {
            continue;
        }

        // Look for aw-* executables in this directory
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();

                // Skip if not a file or directory
                if let Ok(metadata) = fs::metadata(&path) {
                    let file_name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(name) => name.to_string(),
                        None => continue,
                    };

                    // Process only items starting with "aw-"
                    if !file_name.starts_with("aw-") {
                        continue;
                    }

                    // If it's a directory starting with "aw-", add to search stack
                    if metadata.is_dir() {
                        dirs_to_search.push(path);
                    }
                    // If it's an executable file
                    else if metadata.is_file() && file_name.ends_with(".exe") {
                        // Extract name without .exe suffix
                        let name = match file_name.strip_suffix(".exe") {
                            Some(name) => name.to_lowercase(),
                            None => continue,
                        };

                        // Skip if excluded
                        if excluded.contains(&name.as_str()) {
                            continue;
                        }

                        found_modules.insert(name, path);
                    }
                }
            }
        }
    }

    found_modules
}
